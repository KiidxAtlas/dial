use super::*;
use std::cell::Cell;

fn files(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn sized(names: &[(&str, u64)]) -> Vec<(String, u64)> {
    names
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), *bytes))
        .collect()
}

#[test]
fn complete_models_never_download() {
    assert_eq!(
        prepare(
            "org/model",
            true,
            |_| Ok("/cached/model".into()),
            |_| panic!("unexpected listing"),
            |_, _| panic!("unexpected download")
        )
        .unwrap(),
        "/cached/model"
    );
}

#[test]
fn missing_mlx_repository_takes_a_snapshot_and_validates() {
    let downloaded = Cell::new(false);
    let path = prepare(
        "org/model",
        true,
        |value| {
            if value == "/snapshot" {
                Ok(value.into())
            } else {
                bail!("missing or partial")
            }
        },
        |_| {
            Ok(files(&[
                "config.json",
                "model.safetensors",
                "model-q4.gguf",
            ]))
        },
        |repo, fetch| {
            assert_eq!(repo, "org/model");
            assert_eq!(fetch, &Fetch::Snapshot);
            downloaded.set(true);
            Ok(vec!["/snapshot".into()])
        },
    )
    .unwrap();
    assert!(downloaded.get());
    assert_eq!(path, "/snapshot");
}

#[test]
fn invalid_paths_do_not_list_or_download() {
    for model in [
        "/missing/model",
        "./model",
        "../model",
        "model",
        "org/model/subdir",
        "org/model/file.gguf",
        "",
    ] {
        assert!(
            prepare(
                model,
                true,
                |_| bail!("missing"),
                |_| panic!("unexpected listing"),
                |_, _| panic!("unexpected download")
            )
            .is_err(),
            "{model:?} must be rejected"
        );
    }
}

#[test]
fn download_failure_and_incomplete_snapshots_are_errors() {
    assert!(
        prepare(
            "org/model",
            true,
            |_| bail!("missing"),
            |_| Ok(files(&["model.safetensors"])),
            |_, _| bail!("network failure")
        )
        .unwrap_err()
        .to_string()
        .contains("network failure")
    );
    assert!(
        prepare(
            "org/model",
            true,
            |_| bail!("missing"),
            |_| Ok(files(&["model.safetensors"])),
            |_, _| Ok(vec!["/partial".into()])
        )
        .unwrap_err()
        .to_string()
        .contains("no complete usable weights")
    );
}

#[test]
fn single_gguf_downloads_only_that_file_and_its_projector() {
    let names = files(&["README.md", "m-Q4_K_M.gguf", "mmproj-f16.gguf"]);
    assert_eq!(
        plan("org/gguf", &names).unwrap(),
        Fetch::Files(files(&["m-Q4_K_M.gguf", "mmproj-f16.gguf"]))
    );
    assert_eq!(
        plan("org/gguf", &files(&["m-Q4_K_M.gguf"])).unwrap(),
        Fetch::Files(files(&["m-Q4_K_M.gguf"]))
    );
}

#[test]
fn multiple_quantizations_are_never_chosen_or_downloaded() {
    let names = files(&["m-Q4_K_M.gguf", "m-Q8_0.gguf", "mmproj-f16.gguf"]);
    let error = plan("org/gguf", &names).unwrap_err().to_string();
    assert!(error.contains("2 GGUF quantizations"), "{error}");
    assert!(error.contains("m-Q8_0.gguf"), "{error}");
    assert!(
        error.contains("huggingface-cli download org/gguf <file>"),
        "{error}"
    );
    assert!(error.contains("press / to search"), "{error}");
    let downloaded = Cell::new(false);
    assert!(
        prepare(
            "org/gguf",
            true,
            |_| bail!("missing"),
            |_| Ok(names.clone()),
            |_, _| {
                downloaded.set(true);
                Ok(vec![])
            }
        )
        .is_err()
    );
    assert!(!downloaded.get());
}

#[test]
fn complete_split_sets_download_together_and_partial_sets_fail() {
    assert_eq!(
        plan(
            "org/big",
            &files(&["q/m-00002-of-00002.gguf", "q/m-00001-of-00002.gguf"])
        )
        .unwrap(),
        Fetch::Files(files(&[
            "q/m-00001-of-00002.gguf",
            "q/m-00002-of-00002.gguf"
        ]))
    );
    let error = plan(
        "org/big",
        &files(&["m-00001-of-00003.gguf", "m-00002-of-00003.gguf"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("incomplete GGUF split set"), "{error}");
    assert!(plan("org/big", &files(&["m-00001-of-00002.gguf"])).is_err());
    assert!(plan("org/big", &files(&["m-00002-of-00002.gguf"])).is_err());
    // Two different split sets are two quantizations, not one model.
    assert!(
        plan(
            "org/big",
            &files(&["a-00001-of-00002.gguf", "b-00001-of-00001.gguf"])
        )
        .is_err()
    );
}

#[test]
fn ambiguous_projectors_and_missing_weights_are_explained() {
    let error = plan(
        "org/vl",
        &files(&["m.gguf", "mmproj-f16.gguf", "mmproj-f32.gguf"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("2 multimodal projectors"), "{error}");
    let error = plan("org/none", &files(&["model.bin", "README.md"]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no MLX safetensors or GGUF weights"),
        "{error}"
    );
    assert!(error.contains("docs/model-downloads.md"), "{error}");
}

#[test]
fn mixed_mlx_repository_ignores_gguf_by_snapshot() {
    assert_eq!(
        plan(
            "org/mix",
            &files(&["model.safetensors", "a-Q4.gguf", "a-Q8.gguf"])
        )
        .unwrap(),
        Fetch::Snapshot
    );
}

#[test]
fn picked_file_skips_the_cache_lookup_and_fetches_only_that_file() {
    // Another quantization may already be cached, so the cache must not answer for a picked file.
    let looked_up = Cell::new(0);
    let resolved = prepare(
        "org/gguf",
        false,
        |value| {
            looked_up.set(looked_up.get() + 1);
            Ok(value.into())
        },
        |_| Ok(files(&["m-Q8_0.gguf"])),
        |repo, fetch| {
            assert_eq!(repo, "org/gguf");
            assert_eq!(fetch, &Fetch::Files(files(&["m-Q8_0.gguf"])));
            Ok(vec!["/snap/m-Q8_0.gguf".into()])
        },
    )
    .unwrap();
    assert_eq!(resolved, "/snap/m-Q8_0.gguf");
    // Only the final resolve of the downloaded path ran; the repository lookup did not.
    assert_eq!(looked_up.get(), 1);
}

#[test]
fn quant_options_list_each_gguf_quantization_smallest_first() {
    let options = quant_options(&sized(&[
        ("README.md", 10),
        ("m-Q8_0.gguf", 800),
        ("m-Q4_K_M.gguf", 400),
    ]));
    let labels: Vec<_> = options.iter().map(|q| q.label.as_str()).collect();
    assert_eq!(labels, ["m-Q4_K_M", "m-Q8_0"]);
    assert_eq!(options[0].files, files(&["m-Q4_K_M.gguf"]));
    assert_eq!(options[0].bytes, 400);
    assert_eq!(options[1].bytes, 800);
}

#[test]
fn a_picked_quant_is_exactly_what_the_planner_accepts() {
    let listing = sized(&[
        ("m-Q4_K_M.gguf", 400),
        ("m-Q8_0.gguf", 800),
        ("mmproj-f16.gguf", 50),
    ]);
    let options = quant_options(&listing);
    let q8 = options.iter().find(|q| q.label == "m-Q8_0").unwrap();
    // The projector travels with the one attached model; the total includes it.
    assert_eq!(q8.files, files(&["m-Q8_0.gguf", "mmproj-f16.gguf"]));
    assert_eq!(q8.bytes, 850);
    assert_eq!(
        plan("org/gguf", &q8.files).unwrap(),
        Fetch::Files(files(&["m-Q8_0.gguf", "mmproj-f16.gguf"]))
    );
}

#[test]
fn split_sets_are_one_quantization_and_projectors_are_not_options() {
    let options = quant_options(&sized(&[
        ("q/big-00001-of-00002.gguf", 500),
        ("q/big-00002-of-00002.gguf", 500),
        ("small.gguf", 100),
        ("mmproj-a.gguf", 5),
        ("mmproj-b.gguf", 5),
    ]));
    assert_eq!(options.len(), 2, "{options:?}");
    assert_eq!(options[0].label, "small");
    assert_eq!(
        options[0].files,
        files(&["small.gguf"]),
        "no projector when several exist"
    );
    assert_eq!(options[1].label, "big");
    assert_eq!(
        options[1].files,
        files(&["q/big-00001-of-00002.gguf", "q/big-00002-of-00002.gguf"])
    );
    assert_eq!(options[1].bytes, 1000);
}

#[test]
fn repositories_without_gguf_offer_no_quantizations() {
    assert!(quant_options(&sized(&[("model.safetensors", 9), ("README.md", 1)])).is_empty());
    assert!(quant_options(&sized(&[("mmproj-f16.gguf", 9)])).is_empty());
}

#[test]
fn search_results_parse_counts_and_skip_invalid_ids() {
    let json = r#"[
            {"id":"mlx-community/Qwen2.5-Coder-7B-Instruct-4bit","downloads":255850,"likes":30,"tags":["mlx"]},
            {"id":"not a repo","downloads":1},
            {"id":"org/no-counts"}
        ]"#;
    let hits = parse_search(Family::Mlx, json).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].repo, "mlx-community/Qwen2.5-Coder-7B-Instruct-4bit");
    assert_eq!(hits[0].downloads, Some(255_850));
    assert_eq!(hits[0].likes, Some(30));
    assert_eq!(hits[0].family, Family::Mlx);
    assert_eq!(hits[1].downloads, None, "missing counts stay unknown");
    assert!(parse_search(Family::Gguf, "[]").unwrap().is_empty());
    assert!(parse_search(Family::Gguf, "<html>").is_err());
}

#[test]
fn model_browse_and_search_request_supported_metadata_fields() {
    for family in [Family::Mlx, Family::Gguf] {
        let browse = search_query(family, " \t ");
        assert_eq!(browse, search_query(family, ""));
        assert!(browse.contains(&("filter", family.filter())));
        assert!(browse.contains(&("sort", "downloads")));
        assert!(browse.contains(&("direction", "-1")));
        assert!(browse.contains(&("limit", "30")));
        assert!(!browse.iter().any(|(key, _)| *key == "search"));
        let expansions: Vec<_> = browse
            .iter()
            .filter_map(|(key, value)| (*key == "expand").then_some(*value))
            .collect();
        assert_eq!(
            expansions,
            ["downloads", "likes", "createdAt", "safetensors"]
        );
        let mut expected = browse;
        expected.push(("search", "Qwen coder"));
        assert_eq!(search_query(family, "  Qwen coder  "), expected);
    }
}

#[test]
fn model_list_and_detail_share_exact_metadata_parsing() {
    // Shape verified against /api/models/mlx-community/Qwen3-4B-4bit.
    let json = r#"{"id":"mlx-community/Qwen3-4B-4bit","downloads":12,"likes":3,
            "createdAt":"2025-04-28T21:28:51.000Z","usedStorage":2274445183,
            "safetensors":{"parameters":{"U32":4022272000,"BF16":196096},"total":4022468096}}"#;
    let detail = parse_metadata(Family::Mlx, json).unwrap();
    let list = parse_search(Family::Mlx, &format!("[{json}]")).unwrap();
    assert_eq!(list, vec![detail.clone()]);
    assert_eq!(
        detail.created_at.as_deref(),
        Some("2025-04-28T21:28:51.000Z")
    );
    assert_eq!(detail.size_bytes, Some(2_274_445_183));
    assert_eq!(detail.parameters, Some(4_022_468_096));
    assert_eq!(detail.downloads, Some(12));
    assert_eq!(detail.likes, Some(3));
}

#[test]
fn absent_metadata_is_not_invented_from_names_dates_or_tensor_types() {
    for fields in [
        "",
        r#", "createdAt":null,"usedStorage":null,"safetensors":null"#,
        r#", "createdAt":" ","safetensors":{}"#,
        r#", "lastModified":"2025-04-28T21:28:51.000Z","size":10,
                "safetensors":{"parameters":{"F16":4000000000}}"#,
    ] {
        let json = format!(r#"{{"id":"org/model-4B"{fields}}}"#);
        let hit = parse_metadata(Family::Gguf, &json).unwrap();
        assert_eq!(hit.created_at, None);
        assert_eq!(hit.size_bytes, None);
        assert_eq!(hit.parameters, None);
    }
    let zero = parse_metadata(
        Family::Mlx,
        r#"{"id":"org/model","usedStorage":0,"safetensors":{"total":0}}"#,
    )
    .unwrap();
    assert_eq!(zero.size_bytes, Some(0));
    assert_eq!(zero.parameters, Some(0));
}

#[test]
fn invalid_metadata_types_and_out_of_range_counts_are_rejected() {
    for fields in [
        r#""createdAt":42"#,
        r#""usedStorage":-1"#,
        r#""usedStorage":1.5"#,
        r#""usedStorage":"1024""#,
        r#""usedStorage":18446744073709551616"#,
        r#""safetensors":{"total":-1}"#,
        r#""safetensors":{"total":"4000"}"#,
        r#""safetensors":false"#,
    ] {
        let json = format!(r#"{{"id":"org/model",{fields}}}"#);
        assert!(parse_metadata(Family::Mlx, &json).is_err(), "{json}");
        assert!(parse_search(Family::Mlx, &format!("[{json}]")).is_err());
    }
    assert!(parse_metadata(Family::Mlx, r#"{"id":"not a repo"}"#).is_err());
    assert!(parse_metadata(Family::Mlx, "[]").is_err());
    assert!(metadata(Family::Mlx, "../invalid/repo").is_err());
}

#[test]
fn tree_listing_keeps_files_and_prefers_lfs_sizes() {
    let json = r#"[
            {"type":"directory","path":"q","size":0},
            {"type":"file","path":"q/m-Q4.gguf","size":135,"lfs":{"size":4000,"oid":"x"}},
            {"type":"file","path":"README.md","size":12}
        ]"#;
    assert_eq!(
        parse_tree(json).unwrap(),
        sized(&[("q/m-Q4.gguf", 4000), ("README.md", 12)])
    );
    assert!(parse_tree("{}").is_err());
}

#[test]
fn snapshot_bytes_leave_gguf_weights_out() {
    assert_eq!(
        snapshot_bytes(&sized(&[
            ("model.safetensors", 900),
            ("config.json", 1),
            ("m.gguf", 500)
        ])),
        901
    );
}

#[test]
fn platform_choice_prefers_mlx_only_on_apple_silicon() {
    assert_eq!(default_family_for(true), Family::Mlx);
    assert_eq!(default_family_for(false), Family::Gguf);
    assert_eq!(Family::Mlx.other(), Family::Gguf);
    assert_eq!(Family::Gguf.label(), "GGUF");
}

#[test]
fn curated_entries_are_valid_ids_in_their_family() {
    for family in [Family::Mlx, Family::Gguf] {
        let list = recommended(family)
            .into_iter()
            .chain(recommended_memory(family))
            .collect::<Vec<_>>();
        assert!(!list.is_empty());
        for hit in list {
            assert!(is_repo_id(&hit.repo), "{}", hit.repo);
            assert_eq!(hit.family, family);
            assert_eq!(hit.downloads, None);
            assert_eq!(hit.likes, None);
            assert_eq!(hit.created_at, None);
            assert_eq!(hit.size_bytes, None);
            assert_eq!(hit.parameters, None);
            if family == Family::Gguf {
                assert!(
                    hit.repo.to_ascii_uppercase().ends_with("GGUF"),
                    "{}",
                    hit.repo
                );
            }
        }
    }
}

#[test]
fn parameter_hints_come_from_the_name_only() {
    assert_eq!(
        params_hint("mlx-community/Qwen3-4B-4bit").as_deref(),
        Some("4B")
    );
    assert_eq!(
        params_hint("mlx-community/Qwen2.5-Coder-7B-Instruct-4bit").as_deref(),
        Some("7B")
    );
    assert_eq!(params_hint("org/Llama-3.2-1.5b").as_deref(), Some("1.5B"));
    assert_eq!(params_hint("org/plain-model"), None);
    assert_eq!(params_hint("org/4bit-tool"), None);
}

#[test]
fn a_cancelled_listing_is_reported_as_cancelled() {
    // The interpreter cannot start, and the cancel flag is already set: the error names the cancel.
    let cancel = AtomicBool::new(true);
    let error = ensure_local_model_with(
        Path::new("/definitely/not/python"),
        "org/model",
        &Choice::Auto,
        &cancel,
        &|_: &str| {},
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("download cancelled"),
        "{error:#}"
    );
}

#[cfg(unix)]
#[test]
fn subprocess_plans_gguf_files_and_validates_them_before_caching() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let snapshot = temp.path().join("snapshot");
    fs::create_dir(&snapshot).unwrap();
    let model = snapshot.join("m-Q4_K_M.gguf");
    fs::write(&model, b"GGUFweights").unwrap();
    let marker = temp.path().join("fetched");
    let python = temp.path().join("python");
    let json = serde_json::to_string(&model.to_string_lossy()).unwrap();
    // The fake interpreter answers the listing script and records any file fetch.
    fs::write(
        &python,
        format!(
            "#!/bin/sh\n[ \"$1\" = '-c' ] || exit 2\ncase \"$2\" in\n*list_repo_files*) printf '%s\\n' '[\"m-Q4_K_M.gguf\",\"README.md\"]' ;;\n*hf_hub_download*) touch '{marker}'; printf '[%s]\\n' '{json}' ;;\n*) exit 4 ;;\nesac\n",
            marker = marker.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
    let cancel = AtomicBool::new(false);
    let fetch = |repo: &str, fetch: &Fetch| -> Result<Vec<String>> {
        let output = match fetch {
            Fetch::Snapshot => bail!("unexpected snapshot"),
            Fetch::Files(files) => {
                let mut args = vec![repo.to_owned()];
                args.extend(files.iter().cloned());
                run_python(&python, FILES, &args, 10, &cancel, &|_| {})?
            }
        };
        Ok(serde_json::from_str(output.trim())?)
    };
    let resolved = prepare(
        "org/gguf",
        true,
        |value| {
            if Path::new(value).is_absolute() && value.ends_with(".gguf") {
                crate::llm::local_format::gguf_file(Path::new(value))
                    .map(|path| path.to_string_lossy().into_owned())
                    .context("not a GGUF")
            } else {
                bail!("missing")
            }
        },
        |repo| {
            let output = run_python(&python, LIST, &[repo.to_owned()], 10, &cancel, &|_| {})?;
            Ok(serde_json::from_str(output.trim())?)
        },
        fetch,
    )
    .unwrap();
    assert_eq!(
        fs::canonicalize(&resolved).unwrap(),
        fs::canonicalize(&model).unwrap()
    );
    assert!(marker.exists());

    // Multi-quant repositories are rejected by the listing alone: no fetch marker is created.
    fs::remove_file(&marker).unwrap();
    fs::write(
        &python,
        "#!/bin/sh\necho '[\"m-Q4.gguf\",\"m-Q8.gguf\"]'\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
    let error = ensure_local_model(&python, "org/multi-quant").unwrap_err();
    assert!(
        format!("{error:#}").contains("2 GGUF quantizations"),
        "{error:#}"
    );
    assert!(!marker.exists());
}

#[test]
fn image_search_filters_declared_mlx_text_to_image_capabilities() {
    let params = image_search_query("  flux  ");
    assert!(params.contains(&("filter", "mlx")));
    assert!(params.contains(&("pipeline_tag", "text-to-image")));
    assert!(params.contains(&("search", "flux")));
    assert!(
        !image_search_query("")
            .iter()
            .any(|(key, _)| *key == "search")
    );
    let hits = parse_search(Family::Mlx, r#"[{"id":"org/generator"}]"#).unwrap();
    assert_eq!(hits[0].family, Family::Mlx);
}
