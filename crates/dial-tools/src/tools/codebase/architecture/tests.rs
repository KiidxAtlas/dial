use super::*;
#[test]
fn json_object_strips_fences_and_prose() {
    assert_eq!(json_object("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(json_object("Here you go: {\"a\":1} done"), "{\"a\":1}");
    assert_eq!(json_object("no braces"), "no braces");
}
#[test]
fn architecture_draft_without_unknowns_still_parses() {
    let draft: Draft = serde_json::from_value(json!({
        "sections": [{"id":"purpose","kind":"purpose","title":"Purpose","summary":"Routes requests.","sources":["main.rs"]}]
    }))
    .unwrap();
    assert_eq!(draft.sections.len(), 1);
    assert!(draft.unknowns.is_empty());
}
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Codebase, Workspace) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("main.rs"),
        "mod routing; fn main() { routing::run(); }\n",
    )
    .unwrap();
    fs::write(project.path().join("routing.rs"), "pub fn run() {}\n").unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("state")).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    let ws = Workspace::new(project.path()).unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    (home, project, cache, ws)
}
fn answer() -> Value {
    json!({"sections":[
    {"id":"purpose","kind":"purpose","title":"Application","summary":"Application starts routing.","sources":["main.rs"]},
    {"id":"router","kind":"component","title":"Routing owner","summary":"Routing is implemented by run in routing.rs.","sources":["routing.rs"]}
],"unknowns":["Storage behavior is not shown."]})
}
#[test]
fn architecture_kind_formatting_is_canonicalized() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    let mut draft = answer();
    draft["sections"][0]["kind"] = json!(" Purpose ");
    draft["sections"][1]["kind"] = json!("COMPONENT");
    for (id, kind) in [("tokens", "Design_System"), ("styles", "design system")] {
        draft["sections"].as_array_mut().unwrap().push(json!({
            "id":id,"kind":kind,"title":id,"summary":"Styling is not evidenced.",
            "sources":["routing.rs"]
        }));
    }
    cache
        .save_architecture(&packet, &draft.to_string(), "model")
        .unwrap();
    let report = cache.architecture_report().unwrap();
    for section in report["sections"].as_array().unwrap() {
        let expected = match section["id"].as_str().unwrap() {
            "purpose" => "purpose",
            "router" => "component",
            _ => "design-system",
        };
        assert_eq!(section["kind"], expected);
    }
}

#[test]
fn architecture_accepts_one_section_with_flexible_kind_optional_sources_and_extra_fields() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    let draft = json!({"sections":[{
        "id":"overview", "kind":"system overview", "title":"Overview",
        "summary":"The indexed project routes requests.", "extra_model_field":true
    }], "extra_top_level":true});
    assert_eq!(
        cache
            .save_architecture(&packet, &draft.to_string(), "model")
            .unwrap(),
        1
    );
    let saved = cache.architecture_saved().unwrap().unwrap();
    assert_eq!(saved.sections.len(), 1);
    assert_eq!(saved.sections[0].kind, "system-overview");
    assert!(saved.sections[0].sources.is_empty());
    let context = cache.architecture_context("routing", 4000).unwrap();
    assert!(
        context.contains("The indexed project routes requests."),
        "{context}"
    );
    fs::write(_project.path().join("routing.rs"), "pub fn changed() {}\n").unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][0]["status"],
        "stale"
    );
    assert!(
        !cache
            .architecture_context("routing", 4000)
            .unwrap()
            .contains("The indexed project routes requests.")
    );
    assert!(SYSTEM.contains("even one is useful"));
    assert!(!SYSTEM.contains("2-20 sections"));
}

#[test]
fn architecture_synthesis_coerces_integer_values_to_strings() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    let draft = json!({"sections":[{
        "id":"overview", "kind":"system", "title":1,
        "summary":"The indexed project routes requests.", "sources":[]
    }], "unknowns":[1]});

    assert_eq!(
        cache
            .save_architecture(&packet, &draft.to_string(), "model")
            .unwrap(),
        1
    );
    let saved = cache.architecture_saved().unwrap().unwrap();
    assert_eq!(saved.sections[0].title, "1");
    assert_eq!(saved.unknowns, ["1"]);
}
#[test]
fn structure_placement_and_design_system_sections_are_grounded_and_retrievable() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    assert!(
        packet
            .prompt
            .contains("Folder membership (not responsibility)")
    );
    assert!(SYSTEM.contains("Distinguish documented placement rules"));
    let mut draft = answer();
    for (id, kind, summary) in [
        (
            "layout",
            "structure",
            "main.rs delegates to routing.rs; confirm the current call path.",
        ),
        (
            "placement",
            "placement",
            "Extend routing.rs for routing responsibilities; main.rs owns startup.",
        ),
        (
            "style",
            "design-system",
            "No theme or shared styling ownership is evidenced by routing.rs.",
        ),
    ] {
        draft["sections"].as_array_mut().unwrap().push(json!({
            "id":id,"kind":kind,"title":id,"summary":summary,"sources":["routing.rs"]
        }));
    }
    cache
        .save_architecture(&packet, &draft.to_string(), "model")
        .unwrap();
    let context = cache
        .architecture_context("routing placement style layout", 8000)
        .unwrap();
    assert!(context.contains("Extend routing.rs"));
    assert!(context.contains("No theme"));
    assert!(context.contains("main.rs delegates"));
    let report = cache.architecture_report().unwrap();
    assert_eq!(report["sections"].as_array().unwrap().len(), 5);
}

#[test]
fn bounded_packet_lists_only_citable_excerpt_paths() {
    let (_home, project, cache, ws) = fixture();
    for i in 0..60 {
        fs::write(
            project.path().join(format!("file_{i}.rs")),
            "pub fn run() {}\n",
        )
        .unwrap();
    }
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let packet = cache.architecture_synthesis(4096, true).unwrap().unwrap();
    assert!(packet.prompt.len() <= 4096);
    assert!(packet.sources.len() < packet.indexed_files);
    let allowed = packet
        .prompt
        .lines()
        .find_map(|line| line.strip_prefix("Allowed citation paths: "))
        .unwrap();
    let paths: Vec<String> = serde_json::from_str(allowed).unwrap();
    assert_eq!(paths, packet.sources.keys().cloned().collect::<Vec<_>>());
    for path in &paths {
        assert!(packet.prompt.lines().any(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .is_some_and(|row| row["path"] == *path && row["excerpt"].is_string())
        }));
    }
    let omitted = cache
        .freshness_snapshot()
        .unwrap()
        .files
        .keys()
        .find(|path| !packet.sources.contains_key(*path))
        .unwrap()
        .clone();
    let mut draft = answer();
    draft["sections"][0]["sources"] = json!([omitted]);
    assert!(
        cache
            .save_architecture(&packet, &draft.to_string(), "model")
            .is_err()
    );
}
#[test]
fn synthesis_is_incremental_and_rejects_unseen_or_changed_evidence() {
    let (_home, project, cache, ws) = fixture();
    let packet = cache.architecture_synthesis(8000, false).unwrap().unwrap();
    let mut invented = answer();
    invented["sections"][0]["sources"] = json!(["secrets.rs"]);
    assert!(
        cache
            .save_architecture(&packet, &invented.to_string(), "model")
            .is_err()
    );
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_none());
    assert!(
        cache
            .architecture_context("routing", 4000)
            .unwrap()
            .contains("Routing is implemented")
    );
    fs::write(project.path().join("routing.rs"), "pub fn stop() {}\n").unwrap();
    assert!(
        cache
            .save_architecture(&packet, &answer().to_string(), "model")
            .is_err()
    );
    let report = cache.architecture_report().unwrap();
    assert_eq!(report["sections"][0]["status"], "stale");
    assert_eq!(report["sections"][0]["source_status"], "unchanged");
    assert_eq!(report["sections"][1]["status"], "stale");
    assert!(
        !cache
            .architecture_context("routing", 4000)
            .unwrap()
            .contains("Routing is implemented")
    );
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let update = cache.architecture_synthesis(8000, false).unwrap().unwrap();
    assert_eq!(update.retained.len(), 0);
    cache.mark_architecture_attempt(&update).unwrap();
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_none());
    cache.defer_architecture_attempt(&update).unwrap();
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_some());
    assert!(cache.architecture_synthesis(8000, true).unwrap().is_some());
}
#[test]
fn synthesis_rejects_concurrent_replacement_and_respects_context_bounds() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(4096, false).unwrap().unwrap();
    assert!(packet.prompt.len() <= 4096);
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    assert!(
        cache
            .save_architecture(&packet, &answer().to_string(), "model")
            .is_err()
    );
    assert!(cache.architecture_context("routing", 512).unwrap().len() <= 512);
    assert!(
        cache
            .impact_context(&["routing.rs".into()], 2000)
            .unwrap()
            .contains("main.rs")
    );
    let json = cache
        .understanding("architecture", &AtomicBool::new(false))
        .unwrap();
    assert_eq!(json["synthesis"]["revision"], 1);
    assert!(render_understanding(&json, 40).contains("Routing is implemented"));
}

#[test]
fn new_files_branch_and_uncited_configuration_invalidate_claims() {
    let (_home, project, cache, ws) = fixture();
    fs::write(project.path().join("Cargo.lock"), "version = 3\n").unwrap();
    fs::create_dir(project.path().join(".git")).unwrap();
    fs::write(project.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let packet = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    fs::write(project.path().join("Cargo.lock"), "version = 4\n").unwrap();
    let report = cache.architecture_report().unwrap();
    assert_eq!(report["sections"][1]["source_status"], "unchanged");
    assert_eq!(report["sections"][1]["status"], "stale");
    fs::write(project.path().join("Cargo.lock"), "version = 3\n").unwrap();
    fs::write(project.path().join("bypass.rs"), "pub fn bypass() {}\n").unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "stale"
    );
    fs::remove_file(project.path().join("bypass.rs")).unwrap();
    fs::write(project.path().join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "stale"
    );
}

#[test]
fn edit_gate_and_freshness_ignore_unrelated_changes() {
    let (_home, project, cache, ws) = fixture();
    fs::write(project.path().join("other.rs"), "pub fn other() {}\n").unwrap();
    fs::write(project.path().join("README.md"), "# notes\n").unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    // Changing an unrelated source and a prose document leaves both sections fresh.
    fs::write(project.path().join("other.rs"), "pub fn changed() {}\n").unwrap();
    fs::write(project.path().join("README.md"), "# changed\n").unwrap();
    let report = cache.architecture_report().unwrap();
    assert_eq!(report["sections"][1]["status"], "fresh");
    assert!(cache.pre_edit_context("other.rs").unwrap().is_none());
    assert!(cache.pre_edit_context("routing.rs").unwrap().is_none());
    // A claim citing routing.rs blocks edits to routing.rs only once that file changed.
    fs::write(project.path().join("routing.rs"), "pub fn stop() {}\n").unwrap();
    assert!(cache.pre_edit_context("routing.rs").unwrap().is_some());
    assert!(cache.pre_edit_context("other.rs").unwrap().is_none());
}

#[test]
fn scoped_components_survive_unrelated_content_changes_but_not_consumers() {
    let (_home, project, cache, ws) = fixture();
    fs::create_dir(project.path().join("owner")).unwrap();
    fs::create_dir(project.path().join("unrelated")).unwrap();
    fs::write(
        project.path().join("owner/auth.rs"),
        "pub fn authorize() {}\n",
    )
    .unwrap();
    fs::write(
        project.path().join("unrelated/math.rs"),
        "pub fn add() {}\n",
    )
    .unwrap();
    fs::write(project.path().join("consumer.rs"), "use owner::auth;\n").unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let mut draft = answer();
    draft["sections"][1]["sources"] = json!(["owner/auth.rs"]);
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &draft.to_string(), "model")
        .unwrap();
    fs::write(
        project.path().join("unrelated/math.rs"),
        "pub fn subtract() {}\n",
    )
    .unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "fresh"
    );
    fs::write(project.path().join("consumer.rs"), "pub fn bypass() {}\n").unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "stale"
    );
}

#[test]
fn existing_unrelated_file_becoming_consumer_invalidates_saved_scope() {
    let (_home, project, cache, ws) = fixture();
    fs::create_dir(project.path().join("owner")).unwrap();
    fs::create_dir(project.path().join("other")).unwrap();
    fs::write(
        project.path().join("owner/auth.rs"),
        "pub fn authorize() {}\n",
    )
    .unwrap();
    fs::write(
        project.path().join("other/consumer.rs"),
        "pub fn unrelated() {}\n",
    )
    .unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    cache
        .remember(
            "auth owner",
            "Auth owns authorization",
            &["owner/auth.rs".into()],
            "session",
            0,
        )
        .unwrap();
    let mut draft = answer();
    draft["sections"][1]["sources"] = json!(["owner/auth.rs"]);
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &draft.to_string(), "model")
        .unwrap();
    fs::write(
        project.path().join("other/consumer.rs"),
        "pub fn still_unrelated() {}\n",
    )
    .unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "fresh"
    );
    assert!(
        cache
            .context("auth owner", 8000)
            .unwrap()
            .contains("Auth owns authorization")
    );
    fs::write(
        project.path().join("other/consumer.rs"),
        "use owner::auth;\npub fn consume() { auth::authorize(); }\n",
    )
    .unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let report = cache.architecture_report().unwrap();
    assert_eq!(report["sections"][1]["source_status"], "unchanged");
    assert_eq!(report["sections"][1]["status"], "stale");
    assert!(
        !cache
            .context("auth owner", 8000)
            .unwrap()
            .contains("Auth owns authorization")
    );
    assert!(cache.pre_edit_context("owner/auth.rs").unwrap().is_some());
}

#[test]
fn contradiction_blocks_reuse_and_cannot_be_overwritten_by_old_synthesis() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    let older = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .invalidate_architecture(&["router".into()], "Current caller bypasses routing")
        .unwrap();
    assert!(
        cache
            .save_architecture(&older, &answer().to_string(), "model")
            .is_err()
    );
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "contradicted"
    );
    assert!(
        !cache
            .architecture_context("routing", 4000)
            .unwrap()
            .contains("Routing is implemented")
    );
    assert!(cache.pre_edit_context("routing.rs").unwrap().is_some());
    assert!(cache.pre_edit_context("new.rs").unwrap().is_none());
    let new = cache.architecture_synthesis(8000, false).unwrap().unwrap();
    cache
        .save_architecture(&new, &answer().to_string(), "model")
        .unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][1]["status"],
        "fresh"
    );
}

#[test]
#[ignore = "local source-provenance microbenchmark; no model calls"]
fn benchmark_behavior_source_snapshot() {
    let (_home, project, cache, ws) = fixture();
    let source = (0..32)
        .map(|index| format!("pub fn entry_{index}(value: u64) -> u64 {{ value + {index} }}\n"))
        .collect::<String>();
    for index in 0..256 {
        fs::write(project.path().join(format!("module_{index}.rs")), &source).unwrap();
    }
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let expected = cache.freshness_snapshot().unwrap();
    assert!(expected.complete);
    let outline_bytes: usize = expected.outlines.values().map(String::len).sum();
    let mut full_times = Vec::new();
    let mut source_times = Vec::new();
    // Alternate order so one path does not always inherit the warmer filesystem cache.
    for round in 0..20 {
        for full in if round % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            let started = Instant::now();
            let snapshot = if full {
                cache.freshness_snapshot().unwrap()
            } else {
                cache.source_snapshot().unwrap()
            };
            let micros = started.elapsed().as_micros();
            assert_eq!(snapshot.files, expected.files);
            assert_eq!(snapshot.environment, expected.environment);
            assert!(snapshot.complete);
            if full {
                full_times.push(micros);
            } else {
                source_times.push(micros);
            }
        }
    }
    full_times.sort_unstable();
    source_times.sort_unstable();
    eprintln!(
        "source_snapshot files={} rounds=20 outline_bytes_avoided={} full_median_us={} source_only_median_us={}",
        expected.files.len(),
        outline_bytes,
        full_times[10],
        source_times[10]
    );
}

#[test]
fn behavior_check_provenance_does_not_load_dependency_outlines() {
    let (_home, project, cache, _ws) = fixture();
    let full = cache.freshness_snapshot().unwrap();
    let sources = cache.source_snapshot().unwrap();
    assert!(!full.outlines.is_empty());
    assert!(sources.outlines.is_empty());
    assert_eq!(sources.files, full.files);
    assert_eq!(sources.inventory, full.inventory);
    assert_eq!(sources.environment, full.environment);
    assert_eq!(sources.complete, full.complete);

    // An unavailable dependency index must not prevent recording observed check provenance.
    cache
        .conn
        .lock()
        .execute("DROP TABLE codebase_files", [])
        .unwrap();
    cache
        .record_behavior_check("configured tests", true)
        .unwrap();
    let recorded = || -> Value {
        let raw: String = cache
            .conn
            .lock()
            .query_row(
                "SELECT value FROM codebase_meta WHERE key='behavior_check'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    let before = recorded();
    assert_eq!(
        before["source_fingerprint"],
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&full.files).unwrap())
        )
    );
    assert_eq!(before["complete"], true);
    fs::write(project.path().join("routing.rs"), "pub fn changed() {}\n").unwrap();
    cache
        .record_behavior_check("configured tests", false)
        .unwrap();
    let after = recorded();
    assert_ne!(before["source_fingerprint"], after["source_fingerprint"]);
    assert_eq!(after["passed"], false);
}

#[test]
fn legacy_partial_and_behavioral_evidence_never_certify_claims() {
    let (_home, project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    cache
        .record_behavior_check("configured tests", true)
        .unwrap();
    let report = cache.architecture_report().unwrap();
    assert_eq!(
        report["behavioral_evidence"]["status"],
        "passed_for_recorded_sources"
    );
    assert_eq!(report["sections"][1]["behavior_status"], "unverified");
    fs::write(project.path().join("routing.rs"), "pub fn changed() {}\n").unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["behavioral_evidence"]["status"],
        "stale"
    );
    cache
        .record_behavior_check("configured tests", false)
        .unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["behavioral_evidence"]["status"],
        "failed_requires_review"
    );
    let mut saved = cache.architecture_saved().unwrap().unwrap();
    saved.sections[1].freshness = None;
    assert_eq!(
        cache.section_statuses(&saved.sections).unwrap()["router"],
        "stale"
    );
    fs::write(project.path().join("routing.rs"), "pub fn run() {}\n").unwrap();
    assert_eq!(
        cache.section_statuses(&saved.sections).unwrap()["router"],
        "unchecked"
    );
    fs::write(
        project.path().join("oversized.rs"),
        "x".repeat(MAX_FILE as usize + 1),
    )
    .unwrap();
    assert_eq!(
        cache.architecture_report().unwrap()["sections"][0]["status"],
        "unchecked"
    );
}

#[test]
fn findings_watch_uncited_sources_and_require_revision_after_contradiction() {
    let (_home, project, cache, _ws) = fixture();
    cache
        .remember(
            "routing responsibility",
            "Routing owns dispatch",
            &["routing.rs".into()],
            "session",
            0,
        )
        .unwrap();
    assert!(
        cache
            .context("routing", 8000)
            .unwrap()
            .contains("Routing owns dispatch")
    );
    cache
        .invalidate_finding(
            "routing responsibility",
            "Caller disproves this responsibility",
        )
        .unwrap();
    assert!(
        !cache
            .context("routing", 8000)
            .unwrap()
            .contains("Routing owns dispatch")
    );
    assert!(
        cache
            .remember(
                "routing responsibility",
                "Updated finding",
                &["routing.rs".into()],
                "session",
                1
            )
            .is_err()
    );
    cache
        .remember(
            "routing responsibility",
            "Updated finding",
            &["routing.rs".into()],
            "session",
            2,
        )
        .unwrap();
    fs::write(project.path().join("main.rs"), "fn main() {}\n").unwrap();
    assert!(
        !cache
            .context("routing", 8000)
            .unwrap()
            .contains("Updated finding")
    );
    let report = cache
        .understanding("routing responsibility", &AtomicBool::new(false))
        .unwrap();
    assert_eq!(report["findings"][0]["status"], "stale");
}
#[test]
fn clear_architecture_removes_saved_synthesis_and_attempt_marker() {
    let (_home, _project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(16000, true).unwrap().unwrap();
    cache.mark_architecture_attempt(&packet).unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    assert!(cache.architecture_saved().unwrap().is_some());

    assert_eq!(cache.clear_architecture().unwrap(), 2);
    assert!(cache.architecture_saved().unwrap().is_none());
    assert!(
        cache
            .architecture_synthesis(16000, false)
            .unwrap()
            .is_some()
    );
}

#[test]
fn legacy_scope_without_edge_witness_is_unchecked_but_still_detects_changes() {
    let snapshot = Snapshot {
        files: BTreeMap::from([("auth.rs".into(), "digest".into())]),
        inventory: String::new(),
        environment: "environment".into(),
        complete: true,
        outlines: BTreeMap::new(),
    };
    let dependencies = vec![Source {
        path: "auth.rs".into(),
        digest: "digest".into(),
    }];
    let mut scope: Freshness = serde_json::from_value(json!({
        "version": 2,
        "inventory": Codebase::local_inventory(&snapshot, &dependencies),
        "environment": snapshot.environment,
        "dependencies": dependencies,
        "complete": true
    }))
    .unwrap();
    assert_eq!(Codebase::scope_status(Some(&scope), &snapshot), "unchecked");
    scope.dependencies[0].digest = "old digest".into();
    assert_eq!(Codebase::scope_status(Some(&scope), &snapshot), "stale");
}

#[test]
fn deleted_sources_are_stale_and_missing_even_when_the_snapshot_is_partial() {
    let (_home, project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    // An oversized file makes the freshness snapshot partial ("unchecked" before).
    fs::write(
        project.path().join("oversized.rs"),
        "x".repeat(MAX_FILE as usize + 1),
    )
    .unwrap();
    fs::remove_file(project.path().join("routing.rs")).unwrap();
    let report = cache.architecture_report().unwrap();
    let citing: Vec<_> = report["sections"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|section| {
            section["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|source| source["path"] == "routing.rs")
        })
        .collect();
    assert!(!citing.is_empty(), "{report}");
    for section in citing {
        assert_eq!(section["status"], "stale", "{section}");
        assert_eq!(section["source_status"], "missing", "{section}");
    }
}

#[test]
fn impact_hints_rank_imports_before_lexical_names_and_match_whole_identifiers() {
    let (_home, project, cache, ws) = fixture();
    fs::create_dir_all(project.path().join("src/owning")).unwrap();
    for (path, text) in [
        (
            "src/owning/feature_flags.rs",
            "pub const ACTIVE: bool = true;\n",
        ),
        (
            "src/owning/z_consumer.rs",
            "use crate::owning::feature_flags;\n",
        ),
        ("aaa_lexical.rs", "pub fn feature_flags() {}\n"),
        ("prefix_only.rs", "pub fn feature_flags_backup() {}\n"),
        (
            "word_noise.rs",
            "//! feature flags are unrelated prose\npub fn noise() {}\n",
        ),
        (
            "comment_only.rs",
            "//! feature_flags are unrelated prose\npub fn noise() {}\n",
        ),
    ] {
        fs::write(project.path().join(path), text).unwrap();
    }
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let context = cache
        .impact_context(&["src/owning/feature_flags.rs".into()], 1024)
        .unwrap();
    let direct = context
        .find("src/owning/z_consumer.rs")
        .expect("import is suggested");
    let lexical = context
        .find("aaa_lexical.rs")
        .expect("lexical hint remains distinguishable");
    assert!(
        direct < lexical,
        "an alphabetically earlier weak match must not crowd out an import"
    );
    for unrelated in ["prefix_only.rs", "word_noise.rs", "comment_only.rs"] {
        assert!(
            !context.contains(unrelated),
            "{unrelated} is not an identifier reference"
        );
    }
    assert!(context.len() <= 1024);
    assert!(
        cache
            .impact_context(&["new_unrelated.rs".into()], 1024)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn impact_hints_revalidate_digest_and_index_visibility_without_a_refresh() {
    let (_home, project, cache, ws) = fixture();
    fs::create_dir_all(project.path().join("historical/checkout")).unwrap();
    for path in [
        "changed.rs",
        "deleted.rs",
        "ignored.rs",
        "historical/checkout/old.rs",
    ] {
        fs::write(project.path().join(path), "use crate::routing;\n").unwrap();
    }
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    fs::write(project.path().join("changed.rs"), "pub fn unrelated() {}\n").unwrap();
    fs::remove_file(project.path().join("deleted.rs")).unwrap();
    fs::write(project.path().join(".gitignore"), "/ignored.rs\n").unwrap();
    fs::write(
        project.path().join("historical/checkout/.git"),
        "gitdir: /repo/.git/worktrees/old\n",
    )
    .unwrap();
    let context = cache.impact_context(&["routing.rs".into()], 2000).unwrap();
    assert!(
        context.contains("main.rs"),
        "the current importer stays available"
    );
    for excluded in [
        "changed.rs",
        "deleted.rs",
        "ignored.rs",
        "historical/checkout/old.rs",
    ] {
        assert!(
            !context.contains(excluded),
            "stale/excluded index entry {excluded} leaked into change context"
        );
    }
}

#[test]
fn impact_context_omits_stale_findings_and_generic_filename_noise() {
    let (_home, project, cache, _ws) = fixture();
    let packet = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache
        .save_architecture(&packet, &answer().to_string(), "model")
        .unwrap();
    fs::write(
        project.path().join("routing.rs"),
        "pub fn replacement() {}\n",
    )
    .unwrap();
    let context = cache.impact_context(&["routing.rs".into()], 2000).unwrap();
    assert!(!context.contains("Routing is implemented"));
    assert!(!context.contains("Needs refresh:"));
    assert!(context.contains("main.rs"));
    assert!(
        cache
            .impact_context(&["main.rs".into()], 1024)
            .unwrap()
            .is_empty()
    );
}
