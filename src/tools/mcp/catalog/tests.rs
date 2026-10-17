use super::*;

fn stdio(program: &str) -> McpServerConfig {
    McpServerConfig {
        program: PathBuf::from(program),
        ..Default::default()
    }
}

fn candidate(name: &str, source: Source, inputs: Vec<Input>) -> Candidate {
    Candidate {
        name: name.into(),
        title: name.into(),
        description: String::new(),
        sources: vec![source],
        badges: Vec::new(),
        install: Install::Server(Box::new(stdio("npx"))),
        inputs,
        repository: None,
    }
}

#[test]
fn curated_ids_are_unique_and_run_through_a_known_runtime() {
    let mut ids: Vec<&str> = CURATED.iter().map(|c| c.id).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total);
    for item in CURATED {
        assert!(
            matches!(item.runtime, "npx" | "uvx" | "docker"),
            "{}",
            item.id
        );
        assert_eq!(
            item.id,
            config_name(item.id),
            "{} must be a valid table name",
            item.id
        );
    }
}

#[test]
fn nuxt_is_discoverable_without_registry_access_and_resolves_for_install() {
    let config = Config::default();
    let found = curated_matches("NuXt", &config.user_dir, &config.project_root);
    assert_eq!(found.len(), 1);
    let candidate = resolve(&Endpoints::at("invalid://unused"), &config, &found[0].name).unwrap();
    let server = candidate.server(&BTreeMap::new()).unwrap();
    assert_eq!(server.program, PathBuf::from("npx"));
    assert_eq!(server.args, ["-y", "mcp-remote", "https://nuxt.com/mcp"]);
    assert!(candidate.inputs.is_empty());
}

#[test]
fn curated_required_env_is_an_input_and_blank_values_are_missing() {
    let item = CURATED.iter().find(|c| c.id == "github").unwrap();
    let candidate = curated_candidate(item, Path::new("/u"), Path::new("/p"));
    assert_eq!(candidate.inputs.len(), 1);
    assert!(candidate.inputs[0].secret && candidate.inputs[0].required);
    let blank = BTreeMap::from([("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), "  ".to_string())]);
    assert!(candidate.missing_required(&blank).is_some());
    assert!(candidate.server(&BTreeMap::new()).is_err());
    let filled = BTreeMap::from([(
        "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
        " tok ".to_string(),
    )]);
    let server = candidate.server(&filled).unwrap();
    assert_eq!(
        server
            .env
            .get("GITHUB_PERSONAL_ACCESS_TOKEN")
            .map(String::as_str),
        Some("tok")
    );
}

#[test]
fn filesystem_curated_candidate_appends_the_project_root() {
    let item = CURATED.iter().find(|c| c.id == "filesystem").unwrap();
    let candidate = curated_candidate(item, Path::new("/u"), Path::new("/work"));
    let Install::Server(server) = candidate.install else {
        panic!("curated servers are installable");
    };
    assert_eq!(server.args.last().map(String::as_str), Some("/work"));
    assert_eq!(
        server.env.get("UV_CACHE_DIR").map(String::as_str),
        Some("/u/mcp/uv")
    );
}

#[test]
fn config_names_never_contain_dots_or_slashes() {
    assert_eq!(config_name("memory"), "memory");
    assert_eq!(
        config_name("io.github.github/github-mcp-server"),
        "github-mcp-server"
    );
    assert_eq!(config_name("@Acme/Notes.v2"), "notes-v2");
    assert_eq!(config_name("a//b  c"), "b-c");
    assert_eq!(config_name("///"), "mcp-server");
    assert_eq!(config_name("npm:context-mode"), "context-mode");
    assert_eq!(config_name("npm:@upstash/context7-mcp"), "context7-mcp");
    assert_eq!(config_name("github:headroomlabs-ai/headroom"), "headroom");
}

#[test]
fn name_matches_rank_first_and_keep_listing_order() {
    let mut rows = vec![
        candidate("@someone/memory-context", Source::Smithery, vec![]),
        candidate("@other/unrelated", Source::Smithery, vec![]),
        candidate("npm:context-mode-helper", Source::Npm, vec![]),
        candidate("github:mksglu/context-mode", Source::Github, vec![]),
        candidate("npm:context-mode", Source::Npm, vec![]),
    ];
    rank(&mut rows, "Context Mode");
    let names: Vec<&str> = rows.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "github:mksglu/context-mode",
            "npm:context-mode",
            "npm:context-mode-helper",
            "@someone/memory-context",
            "@other/unrelated",
        ]
    );
}

#[test]
fn dedupe_matches_owner_qualified_names_only() {
    assert_eq!(
        dedupe_key("io.github.github/github-mcp-server"),
        dedupe_key("@github/github-mcp-server")
    );
    assert_eq!(
        dedupe_key("github:mksglu/context-mode"),
        dedupe_key("io.github.mksglu/context-mode")
    );
    assert_eq!(dedupe_key("npm:@acme/notes"), dedupe_key("@acme/notes"));
    assert_ne!(dedupe_key("github"), dedupe_key("io.github.someone/github"));
    assert_ne!(dedupe_key("npm:memory"), dedupe_key("memory"));
}

fn running(mut candidate: Candidate, program: &str, args: &[&str]) -> Candidate {
    candidate.install = Install::Server(Box::new(McpServerConfig {
        program: PathBuf::from(program),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        ..Default::default()
    }));
    candidate
}

#[test]
fn listings_running_the_same_package_fold_but_bridges_do_not() {
    let official = running(
        candidate("io.github.upstash/context7", Source::Official, vec![]),
        "npx",
        &["-y", "@upstash/context7-mcp@1.0.0"],
    );
    let npm = running(
        candidate("npm:@upstash/context7-mcp", Source::Npm, vec![]),
        "npx",
        &["-y", "@upstash/context7-mcp@1.0.2"],
    );
    let curated = running(
        candidate("context7", Source::Curated, vec![]),
        "npx",
        &["-y", "@upstash/context7-mcp"],
    );
    let bridge = running(
        candidate("npm:mcp-remote", Source::Npm, vec![]),
        "npx",
        &["-y", "mcp-remote@0.1.0"],
    );
    let nuxt = running(
        candidate("nuxt", Source::Curated, vec![]),
        "npx",
        &["-y", "mcp-remote", "https://nuxt.com/mcp"],
    );
    let merged = merge(vec![official, npm, bridge, curated, nuxt]);
    let names: Vec<&str> = merged.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["io.github.upstash/context7", "npm:mcp-remote", "nuxt"]
    );
    assert_eq!(
        merged[0].sources,
        vec![Source::Official, Source::Npm, Source::Curated]
    );
}

#[test]
fn a_github_row_folds_into_its_package_unless_the_repository_is_shared() {
    let repo = |mut c: Candidate, url: &str| {
        c.repository = Some(url.into());
        c
    };
    let npm = repo(
        candidate("npm:context-mode", Source::Npm, vec![]),
        "https://github.com/mksglu/context-mode",
    );
    let github = repo(
        candidate("github:MKSGLU/context-mode", Source::Github, vec![]),
        "https://github.com/mksglu/context-mode",
    );
    let merged = merge(vec![npm, github]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].sources, vec![Source::Npm, Source::Github]);

    let shared = "https://github.com/modelcontextprotocol/servers";
    let merged = merge(vec![
        repo(candidate("npm:@mcp/memory", Source::Npm, vec![]), shared),
        repo(candidate("npm:@mcp/fetch", Source::Npm, vec![]), shared),
        repo(
            candidate(
                "github:modelcontextprotocol/servers",
                Source::Github,
                vec![],
            ),
            shared,
        ),
    ]);
    assert_eq!(merged.len(), 3);
}

#[test]
fn repositories_normalize_from_every_common_spelling() {
    let expected = Some("https://github.com/mksglu/context-mode".to_owned());
    for raw in [
        "git+https://github.com/mksglu/context-mode.git",
        "git://github.com/mksglu/context-mode.git",
        "git@github.com:mksglu/context-mode.git",
        "ssh://git@github.com/mksglu/context-mode",
        "github:mksglu/context-mode",
        "mksglu/context-mode",
        "https://github.com/mksglu/context-mode/blob/main/README.md",
        "https://github.com/mksglu/context-mode#readme",
    ] {
        assert_eq!(normalize_repository(raw), expected, "{raw}");
    }
    assert_eq!(normalize_repository("https://docs.example.com"), None);
    assert_eq!(normalize_repository("file:///etc/passwd"), None);
}

#[test]
fn merge_keeps_the_first_install_and_adds_sources_once() {
    let official = candidate("io.github.acme/notes", Source::Official, vec![]);
    let mut smithery = candidate("@acme/notes", Source::Smithery, vec![]);
    smithery.badges.push("verified".into());
    let other = candidate("acme-other", Source::Curated, vec![]);
    let again = candidate("@acme/notes", Source::Smithery, vec![]);
    let merged = merge(vec![official, smithery, other, again]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].name, "io.github.acme/notes");
    assert_eq!(merged[0].sources, vec![Source::Official, Source::Smithery]);
    assert_eq!(merged[0].badges, vec!["verified".to_string()]);
    assert_eq!(merged[1].origin(), Source::Curated);
}

#[test]
fn install_summary_names_the_command_or_the_blocker() {
    let server = McpServerConfig {
        program: PathBuf::from("uvx"),
        args: vec!["mcp-server-git".into(), "--repository".into()],
        ..Default::default()
    };
    let mut c = candidate("git", Source::Curated, vec![]);
    c.install = Install::Server(Box::new(server));
    assert_eq!(c.install_summary(), "uvx mcp-server-git --repository");
    c.install = Install::Blocked("needs an Authorization header".into());
    assert!(c.install_summary().contains("Authorization"));
    assert!(!c.installable());
}

#[test]
fn secret_names_are_detected_loosely() {
    assert!(looks_secret("SLACK_BOT_TOKEN"));
    assert!(looks_secret("apiKey"));
    assert!(!looks_secret("GITLAB_API_URL"));
}
