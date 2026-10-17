use crate::config::subagents::builtin;
use crate::config::subagents::is_reserved;

use super::*;

#[test]
fn fresh_run_and_start_handoffs_match_without_replacing_explicit_context() {
    let config = config(&[]);
    let args = json!({"task":"inspect parser","solution_space":"area audit","context":"explicit background"});
    let control = json!({"original_goal":"repair parser", "working_state":{"claim":"unverified"}});
    let todos: Vec<super::super::todo::Task> = serde_json::from_value(json!([
        {"content":"closed", "status":"completed"},
        {"content":"next", "status":"pending"},
        {"content":"active", "status":"in_progress"}
    ]))
    .unwrap();
    let handoff = Handoff::capture(&control, &todos);
    let mut run = parse(&args, &config).unwrap();
    let mut start_args = args.clone();
    start_args["action"] = json!("start");
    let mut start = parse(&start_args, &config).unwrap();
    handoff.apply(&mut run.jobs);
    handoff.apply(&mut start.jobs);
    let brief = start.jobs[0].brief();
    assert_eq!(run.jobs[0].brief(), brief);
    assert!(brief.contains("repair parser"));
    assert!(brief.contains("recorded claims, not verified"));
    assert!(brief.contains("explicit background"));
    assert!(brief.contains("inspect parser"));
    assert!(!brief.contains("closed"));
    assert!(brief.find("active").unwrap() < brief.find("next").unwrap());

    let long = Handoff::capture(
        &json!({"original_goal":"x".repeat(100_000), "working_state":"y".repeat(100_000)}),
        &todos,
    );
    assert!(long.goal.unwrap().len() < 5000);
    assert!(long.working_state.unwrap().len() < 3000);
}

#[test]
fn resumed_jobs_keep_saved_background_and_conversation() {
    let config = config(&[]);
    let mut request = parse(
        &json!({"task":"follow up", "solution_space":"s", "context":"saved context"}),
        &config,
    )
    .unwrap();
    let saved = json!({"role":"user", "content":"saved brief"});
    request.jobs[0].resume_messages = vec![saved.clone()];
    request.jobs[0].resume_tools = Some(vec!["read".into()]);
    Handoff::capture(&json!({"original_goal":"new parent goal"}), &[]).apply(&mut request.jobs);
    assert_eq!(request.jobs[0].resume_messages, [saved]);
    assert_eq!(request.jobs[0].resume_tools, Some(vec!["read".into()]));
    assert!(request.jobs[0].handoff.goal.is_none());
    assert!(request.jobs[0].brief().contains("saved context"));
    assert!(!request.jobs[0].brief().contains("new parent goal"));
}

#[test]
fn handoff_brief_leads_with_the_current_request_and_labels_the_first_goal_as_history() {
    let config = config(&[]);
    let mut request = parse(
        &json!({"task":"fix the CSS regression", "solution_space":"s"}),
        &config,
    )
    .unwrap();
    let control = json!({
        "original_goal": "explain the repo layout",
        "current_instruction": "fix the CSS regression"
    });
    Handoff::capture(&control, &[]).apply(&mut request.jobs);
    let brief = request.jobs[0].brief();
    assert!(brief.starts_with("Parent's current request"));
    assert!(
        brief.find("fix the CSS regression").unwrap()
            < brief.find("explain the repo layout").unwrap()
    );
    assert!(brief.contains("Session-start context (historical"));
    assert!(!brief.contains("Parent goal (background"));

    // Before any newer instruction, the first goal is still the framing context.
    let first = json!({"original_goal": "explain the repo layout", "current_instruction": "explain the repo layout"});
    Handoff::capture(&first, &[]).apply(&mut request.jobs);
    let brief = request.jobs[0].brief();
    assert_eq!(brief.matches("explain the repo layout").count(), 1);
    assert!(brief.contains("Parent's current request"));
}

#[test]
fn reviewer_preserves_read_only_permissions_and_optional_schema() {
    let config = config(&[]);
    let mut args =
        json!({"agent":"reviewer", "task":"audit parser", "solution_space":"area audit"});
    let request = parse(&args, &config).unwrap();
    let job = &request.jobs[0];
    assert!(job.schema.is_none());
    assert!(!job.spec.mutates);
    assert_eq!(job.spec.tools, ["read", "glob", "grep", "skill"]);
    args["output_schema"] = json!({"type":"object", "required":["verdict"], "properties":{"verdict":{"type":"string"}}});
    let request = parse(&args, &config).unwrap();
    assert_eq!(
        request.jobs[0].schema.as_ref(),
        Some(&args["output_schema"])
    );
}

#[test]
fn synchronous_previews_retain_complete_single_and_batch_reports() {
    use base64::Engine;
    let config = config(&[]);
    let root = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(root.path()).unwrap();
    let report = format!("{}FINAL-EVIDENCE", "évidence ".repeat(4000));
    for batch in [false, true] {
        let args = if batch {
            json!({"tasks":[{"task":"audit", "solution_space":"s"}, {"task":"second", "solution_space":"s"}]})
        } else {
            json!({"task":"audit", "solution_space":"s"})
        };
        let request = parse(&args, &config).unwrap();
        let outcomes = request
            .jobs
            .iter()
            .map(|_| {
                let mut outcome = Outcome::failed(2, "");
                outcome.status = Status::done(report.clone());
                Some(outcome)
            })
            .collect();
        let output = render(&request, outcomes, &BTreeMap::new(), Some(&store));
        assert!(!output.is_error);
        assert!(
            output
                .text
                .contains("Preview only; complete task report: artifact://")
        );
        assert!(!output.text.contains("FINAL-EVIDENCE"));
        let data = output.data.unwrap();
        assert_eq!(data["jobs"][0]["preview"], true);
        let uri = data["artifact"].as_str().unwrap();
        let mut bytes = Vec::new();
        loop {
            let chunk: Value = serde_json::from_str(
                &store
                    .retrieve_bytes(uri, bytes.len() as u64, 16384)
                    .unwrap(),
            )
            .unwrap();
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
            if chunk["next_offset"] == chunk["total_bytes"] {
                break;
            }
        }
        let retained: Value = serde_json::from_slice(&bytes).unwrap();
        for job in retained["jobs"].as_array().unwrap() {
            assert_eq!(job["result"], report);
            assert!(job.get("preview").is_none());
        }
    }
}

#[test]
fn synchronous_retention_failure_and_absence_preserve_full_typed_report() {
    let config = config(&[]);
    let request = parse(&json!({"task":"audit", "solution_space":"s"}), &config).unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(root.path())
        .unwrap()
        .with_limits(&crate::storage::artifact::ArtifactConfig {
            max_bytes: 128,
            head_bytes: 64,
        });
    let report = "é".repeat(20_000);
    for artifacts in [None, Some(&store)] {
        let output = render(
            &request,
            vec![Some(Outcome::failed(2, report.clone()))],
            &BTreeMap::new(),
            artifacts,
        );
        assert!(output.is_error);
        assert!(output.text.contains(&report));
        let data = output.data.unwrap();
        assert_eq!(data["jobs"][0]["result"], report);
        assert!(data.get("artifact").is_none());
        assert!(data["jobs"][0].get("preview").is_none());
        if artifacts.is_some() {
            assert!(
                data["artifact_error"]
                    .as_str()
                    .unwrap()
                    .contains("retention limit")
            );
            assert!(output.text.contains("Artifact retention failed"));
        }
    }
}

#[test]
fn synchronous_structured_results_remain_intact() {
    let config = config(&[]);
    let request = parse(
        &json!({"task":"audit", "solution_space":"s", "output_schema":{"type":"object"}}),
        &config,
    )
    .unwrap();
    let value = json!({"evidence":"x".repeat(30_000)});
    let text = value.to_string();
    let mut outcome = Outcome::failed(2, "");
    outcome.status = Status::done(text.clone());
    outcome.data = Some(value.clone());
    let output = render(&request, vec![Some(outcome)], &BTreeMap::new(), None);
    assert!(!output.is_error);
    assert_eq!(output.text, text);
    assert_eq!(output.data.unwrap()["jobs"][0]["result"], value);
}

fn config(presets: &[(&str, &str)]) -> Config {
    Config {
        subagents: presets
            .iter()
            .map(|(n, p)| (n.to_string(), p.to_string()))
            .collect(),
        ..Config::default()
    }
}

#[test]
fn sibling_jobs_share_system_prompt_and_brief_prefix_up_to_their_assignment() {
    let config = config(&[]);
    let request = parse(
        &json!({
            "tasks": [
                {"task": "audit module a", "solution_space": "s"},
                {"task": "audit module b", "solution_space": "s"}
            ],
            "context": "Shared batch context."
        }),
        &config,
    )
    .unwrap();
    assert!(request.batch);
    let (first, second) = (&request.jobs[0], &request.jobs[1]);
    let tools = ["read", "grep", "codebase_memory"].map(str::to_owned);
    let root = Path::new("/repo");
    // Same kind and no per-item contract: the system prompt is byte-identical across siblings.
    assert_eq!(first.system(root, &tools), second.system(root, &tools));
    // The brief carries the shared context before the per-job scope and assignment.
    let (a, b) = (first.brief(), second.brief());
    let split = a.find("Scope:").unwrap();
    assert_eq!(a[..split], b[..b.find("Scope:").unwrap()]);
    assert!(a[split..].contains("audit module a"));
    assert!(b[split..].contains("audit module b"));
    assert!(a.contains("Shared batch context."));
}

#[test]
fn a_single_prompt_defaults_to_a_scout_with_the_types_step_budget() {
    let config = config(&[]);
    let request = parse(&json!({"task":"look around","solution_space":"s"}), &config).unwrap();
    let job = &request.jobs[0];
    assert!(!request.batch);
    assert_eq!(
        (job.spec.name, job.max_steps, job.label.as_str()),
        ("scout", 0, "scout")
    );
}

#[test]
fn batches_are_validated_whole_and_capped() {
    let config = config(&[]);
    let ok = parse(&json!({"tasks":[{"agent":"reviewer","task":"a","solution_space":"s","name":"first"},{"task":"b","solution_space":"s","max_steps":999}]}), &config).unwrap();
    assert!(ok.batch);
    assert_eq!(
        (ok.jobs[0].label.as_str(), ok.jobs[0].spec.name),
        ("first", "reviewer")
    );
    assert_eq!(
        (ok.jobs[1].label.as_str(), ok.jobs[1].max_steps),
        ("scout#2", 999)
    );
    let bad = |args: Value| parse(&args, &config).err().unwrap();
    assert!(
        bad(json!({"tasks":[{"task":"a","solution_space":"s"},{"agent":"nope","task":"b","solution_space":"s"}]}))
            .contains("tasks[1]: unknown agent \"nope\"")
    );
    assert!(
        bad(
            json!({"tasks":[{"task":"a","solution_space":"s"},{"task":"  ","solution_space":"s"}]})
        )
        .contains("tasks[1]: needs a non-empty `task`")
    );
    assert!(
        bad(json!({"tasks":[{"task":"a"}]}))
            .contains("tasks[0]: needs a non-empty `solution_space`")
    );
    assert!(
        bad(json!({"tasks":(0..MAX_BATCH+1).map(|_| json!({"task":"x","solution_space":"s"})).collect::<Vec<_>>()}))
            .contains(&format!("1 to {MAX_BATCH}"))
    );
    assert!(bad(json!({"task":"a","solution_space":"s","tasks":[]})).contains("not both"));
    assert!(bad(json!({})).contains("`task` or a `tasks`"));
}
#[test]
fn presets_are_custom_scouts_and_types_win_the_name() {
    let config = config(&[("security", "Focus on injection bugs.")]);
    let job = &parse(
        &json!({"task":"audit","solution_space":"s","agent":"security"}),
        &config,
    )
    .unwrap()
    .jobs[0];
    assert_eq!((job.spec.name, job.kind), ("scout", "security"));
    assert!(
        job.system(Path::new("/p"), &["read".into()])
            .contains("Focus on injection bugs.")
    );
    assert!(is_reserved("worker") && !is_reserved("security"));
}

#[test]
fn custom_agents_narrow_a_read_only_base_and_cannot_be_writers() {
    use crate::config::types::AgentConfig;
    let mut cfg = config(&[]);
    cfg.agents.insert(
        "sec".into(),
        AgentConfig {
            base: "reviewer".into(),
            description: "security review".into(),
            prompt: "Look for injection.".into(),
            tools: Some(vec!["read".into(), "grep".into()]),
            max_steps: Some(5),
            spawns: None,
            model: Vec::new(),
            thinking_level: None,
            output: None,
            autoload_skills: Vec::new(),
        },
    );
    cfg.validate().unwrap();
    let job = &parse(
        &json!({"task":"audit","solution_space":"s","agent":"sec"}),
        &cfg,
    )
    .unwrap()
    .jobs[0];
    assert_eq!(
        (job.spec.name, job.kind, job.max_steps),
        ("reviewer", "sec", 5)
    );
    assert_eq!(
        job.allowed_tools,
        Some(&["read".to_owned(), "grep".to_owned()][..])
    );
    assert!(
        schema(&cfg)["function"]["description"]
            .as_str()
            .unwrap()
            .contains("sec: security review")
    );
    // A worker-based custom agent is a writer: it validates, and launching it needs approval
    // (`launches_writer` is fail-closed for custom names).
    let mut writer = cfg.clone();
    writer.agents.get_mut("sec").unwrap().base = "worker".into();
    writer.validate().unwrap();
    let job = &parse(
        &json!({"task":"audit","solution_space":"s","agent":"sec"}),
        &writer,
    )
    .unwrap()
    .jobs[0];
    assert_eq!(job.spec.name, "worker");
    assert!(job.spec.mutates, "a custom worker-based agent is a writer");
    assert!(
        launches_worker(&json!({"agent":"sec","task":"audit","solution_space":"s"})),
        "launching a custom writer needs approval"
    );
    let mut autoloaded = writer.clone();
    autoloaded.agents.get_mut("sec").unwrap().autoload_skills = vec!["db".into()];
    let mut job = parse(
        &json!({"task":"audit","solution_space":"s","agent":"sec"}),
        &autoloaded,
    )
    .unwrap()
    .jobs
    .remove(0);
    assert_eq!(job.autoload_skills, ["db"]);
    assert!(
        !job.system(Path::new("/repo"), &[])
            .contains("PRELOADED SKILLS")
    );
    job.preloaded = "BODY-OF-DB-SKILL".into();
    assert!(
        job.system(Path::new("/repo"), &[])
            .contains("BODY-OF-DB-SKILL")
    );
    let mut bad = cfg.clone();
    bad.agents.get_mut("sec").unwrap().base = "unknown".into();
    assert!(
        bad.validate().is_err(),
        "custom agents need a built-in base"
    );
    let mut bad = cfg.clone();
    bad.agents.get_mut("sec").unwrap().tools = Some(vec!["write".into()]);
    assert!(bad.validate().is_err(), "a custom agent never gains tools");
    let mut bad = cfg;
    bad.agents.insert(
        "scout".into(),
        AgentConfig {
            base: "scout".into(),
            description: String::new(),
            prompt: "x".into(),
            tools: None,
            max_steps: None,
            spawns: None,
            model: Vec::new(),
            thinking_level: None,
            output: None,
            autoload_skills: Vec::new(),
        },
    );
    assert!(bad.validate().is_err());
}

#[test]
fn a_batch_schema_is_the_default_and_an_unusable_schema_is_refused() {
    let config = config(&[]);
    let schema = json!({"type":"object","required":["n"]});
    let request = parse(&json!({"output_schema":schema,"tasks":[{"task":"a","solution_space":"s"},{"task":"b","solution_space":"s","output_schema":{"type":"string"}}]}), &config).unwrap();
    assert_eq!(request.jobs[0].schema.as_ref(), Some(&schema));
    assert_eq!(request.jobs[1].schema, Some(json!({"type":"string"})));
    assert!(
        parse(
            &json!({"task":"a","solution_space":"s","output_schema":{"oneOf":[]}}),
            &config
        )
        .err()
        .unwrap()
        .contains("invalid `output_schema`")
    );
}

#[test]
fn experimental_work_packets_are_off_by_default_and_enforce_handoff_contracts() {
    let args = json!({
        "task":"inspect this change","solution_space":"s",
        "work_packet":{
            "scope":"src/agent/subagents.rs",
            "interface":"Keep existing task argument compatibility.",
            "acceptance":["Existing calls remain valid.","Invalid packets are rejected."]
        }
    });
    let mut config = config(&[]);
    assert!(parse(&args, &config).err().unwrap().contains("work_packet"));
    assert!(schema(&config)["function"]["parameters"]["properties"]["work_packet"].is_null());
    config.experimental.work_packet_contracts = true;
    let request = parse(&args, &config).unwrap();
    let prompt = request.jobs[0].system(Path::new("/repo"), &[]);
    assert!(prompt.contains("WORK PACKET CONTRACT"));
    assert!(prompt.contains("Keep existing task argument compatibility."));
    assert!(prompt.contains("Never claim a check or interface contract passed"));
    assert_eq!(
        request.jobs[0]
            .work_packet
            .as_ref()
            .unwrap()
            .acceptance
            .len(),
        2
    );
    assert!(
        parse(
            &json!({
                "task":"x","solution_space":"s",
                "work_packet":{"scope":"x","interface":"x","acceptance":[]}
            }),
            &config
        )
        .err()
        .unwrap()
        .contains("1 to 16")
    );
}

#[test]
fn experimental_profile_matching_only_accepts_named_operator_profiles() {
    let mut config = config(&[]);
    config.model_profiles.insert(
        "fast".into(),
        crate::llm::profiles::ModelProfile {
            provider: "openai".into(),
            model: "small".into(),
            ..Default::default()
        },
    );
    let args = json!({"task":"summarize","solution_space":"s","model_profile":"fast"});
    assert!(
        parse(&args, &config)
            .err()
            .unwrap()
            .contains("profile_matching")
    );
    assert!(schema(&config)["function"]["parameters"]["properties"]["model_profile"].is_null());
    config.experimental.profile_matching = true;
    assert_eq!(
        schema(&config)["function"]["parameters"]["properties"]["model_profile"]["enum"],
        json!(["fast"])
    );
    assert_eq!(
        parse(&args, &config).unwrap().model_profile.as_deref(),
        Some("fast")
    );
    assert!(
        parse(
            &json!({"task":"x","solution_space":"s","model_profile":"openai/unlisted"}),
            &config
        )
        .err()
        .unwrap()
        .contains("unknown configured model profile")
    );
    let role = config
        .for_role("small")
        .with_named_model_profile("fast")
        .unwrap();
    assert_eq!(role.selection().model, "small");
    assert_eq!(role.model_profile.as_deref(), Some("fast"));
}
#[test]
fn only_worker_launches_need_approval() {
    assert!(!launches_worker(&json!({"task":"x","solution_space":"s"})));
    assert!(!launches_worker(
        &json!({"agent":"reviewer","task":"x","solution_space":"s"})
    ));
    assert!(launches_worker(
        &json!({"agent":"worker","task":"x","solution_space":"s"})
    ));
    assert!(launches_worker(
        &json!({"tasks":[{"agent":"scout","task":"x","solution_space":"s"},{"agent":"worker","task":"y","solution_space":"s"}]})
    ));
}

#[test]
fn role_follows_the_agent_type() {
    let builtin_role = |name: &str| role_for_agent(builtin(name).unwrap(), false);
    assert_eq!(builtin_role("scout"), "small");
    assert_eq!(builtin_role("reviewer"), "tester");
    assert_eq!(builtin_role("planner"), "planner");
    assert_eq!(builtin_role("worker"), "task");
    assert_eq!(builtin_role("test-runner"), "tester");
    assert_eq!(role_for_agent(builtin("scout").unwrap(), true), "small");
    assert_eq!(role_for_agent(builtin("worker").unwrap(), true), "task");
    assert_eq!(role_for_agent(builtin("reviewer").unwrap(), true), "tester");
    assert_eq!(role_for_agent(builtin("planner").unwrap(), true), "planner");
}

#[test]
fn task_defaults_do_not_override_assigned_specialists_or_explicit_routes() {
    use crate::llm::model_config::ModelSelection;
    let mut config = config(&[]);
    config.model = "main-model".into();
    let selection = |model: &str| ModelSelection {
        provider: "openai".into(),
        model: model.into(),
        ..Default::default()
    };
    config
        .model_roles
        .insert("task".into(), selection("task-model"));
    let args = json!({"tasks":[
        {"agent":"worker","task":"implement","solution_space":"s"},
        {"agent":"scout","task":"inspect","solution_space":"s"},
        {"agent":"reviewer","task":"review","solution_space":"s"},
        {"agent":"planner","task":"plan","solution_space":"s"},
        {"agent":"test-runner","task":"test","solution_space":"s"}
    ]});
    let request = parse(&args, &config).unwrap();
    assert!(
        request
            .jobs
            .iter()
            .all(|job| job.role == "task" && job.model_routes[0].config.model == "task-model")
    );
    for role in ["small", "tester", "planner"] {
        config
            .model_roles
            .insert(role.into(), selection(&format!("{role}-model")));
    }
    let request = parse(&args, &config).unwrap();
    let roles: Vec<_> = request.jobs.iter().map(|job| job.role).collect();
    assert_eq!(roles, ["task", "small", "tester", "planner", "tester"]);
    for job in &request.jobs {
        assert_eq!(
            job.model_routes[0].config.model,
            format!("{}-model", job.role)
        );
        assert_eq!(job.model_routes[0].source, "assigned");
    }
    config.experimental.profile_matching = true;
    config.model_profiles.insert(
        "explicit".into(),
        crate::llm::profiles::ModelProfile {
            provider: "openai".into(),
            model: "explicit-model".into(),
            ..Default::default()
        },
    );
    // Merely exposing profiles must never change the configured role.
    assert_eq!(
        parse(&args, &config).unwrap().jobs[0].model_routes[0]
            .config
            .model,
        "task-model"
    );
    let override_args = json!({"agent":"worker","task":"implement","solution_space":"s","model_profile":"explicit"});
    let request = parse(&override_args, &config).unwrap();
    assert_eq!(
        request.jobs[0].model_routes[0].config.model,
        "explicit-model"
    );
    assert_eq!(request.jobs[0].model_routes[0].source, "profile");
    config.model_roles.clear();
    assert_eq!(
        parse(&args, &config).unwrap().jobs[0].model_routes[0]
            .config
            .model,
        "main-model"
    );
}

#[test]
fn task_defaults_use_configured_helper_chains_before_main() {
    use crate::llm::model_config::ModelSelection;
    let helpers = ["task", "small", "coach", "tester", "planner"];
    let cases = [
        ("worker", &["task", "coach", "small"][..]),
        ("scout", &["small", "task"][..]),
        ("reviewer", &["tester", "coach", "task", "small"][..]),
        ("test-runner", &["tester", "coach", "task", "small"][..]),
        ("planner", &["planner", "coach", "task", "small"][..]),
    ];
    // Exercise every assignment combination, including no helpers and all specialists.
    for mask in 0..(1 << helpers.len()) {
        let mut config = config(&[]);
        config.model = "main-model".into();
        for (i, role) in helpers.iter().enumerate() {
            if mask & (1 << i) != 0 {
                config.model_roles.insert(
                    (*role).into(),
                    ModelSelection::new("openai", &format!("{role}-model")),
                );
            }
        }
        for (agent, chain) in cases {
            let expected = chain
                .iter()
                .find(|role| config.model_roles.contains_key(**role));
            let args = json!({"agent":agent,"task":"slice","solution_space":"s"});
            let request = parse(&args, &config).unwrap();
            let route = &request.jobs[0].model_routes[0];
            let expected_model =
                expected.map_or("main-model".to_owned(), |role| format!("{role}-model"));
            assert_eq!(route.config.model, expected_model, "{agent}, mask={mask}");
            if let Some(role) = expected {
                assert_eq!(route.role, *role);
            }
        }
    }
    let mut config = config(&[]);
    config.model = "main-model".into();
    config
        .model_roles
        .insert("coach".into(), ModelSelection::new("openai", "coach-model"));
    config.agents.insert(
        "explicit-main".into(),
        serde_json::from_value(json!({
            "base":"worker", "prompt":"Implement", "model":["@main"]
        }))
        .unwrap(),
    );
    let args = json!({"agent":"explicit-main","task":"slice","solution_space":"s"});
    let request = parse(&args, &config).unwrap();
    assert_eq!(request.jobs[0].model_routes[0].config.model, "main-model");
    assert_eq!(request.jobs[0].model_routes[0].source, "definition");
}

#[test]
fn file_defined_agents_spawn_with_their_base_prompt_and_autoloads() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join(".dial/agents")).unwrap();
    std::fs::write(
        project.join(".dial/agents/migrator.md"),
        "---\ndescription: migrates\ntools: [read, edit]\nautoload_skills: [db]\n---\nMIGRATE-BODY",
    )
    .unwrap();
    let base = Config {
        project_root: project,
        ..Config::default()
    };
    let merged = with_file_agents(&base);
    let request = parse(
        &json!({"agent": "migrator", "task": "run it", "solution_space": "s"}),
        &merged,
    )
    .unwrap();
    let job = &request.jobs[0];
    assert_eq!(job.kind, "migrator");
    assert_eq!(job.spec.name, "worker");
    assert!(job.spec.mutates);
    assert_eq!(job.autoload_skills, ["db"]);
    assert!(job.system(&base.project_root, &[]).contains("MIGRATE-BODY"));
    assert!(launches_worker(
        &json!({"agent": "migrator", "task": "run it", "solution_space": "s"})
    ));
}

#[test]
fn the_harness_can_withhold_tools_from_every_type() {
    let enabled: Vec<String> = ["read", "grep"].map(String::from).into();
    let ext = Registry::new();
    assert_eq!(
        toolset(builtin("scout").unwrap(), &enabled, &ext),
        ["read", "grep"]
    );
    assert!(!toolset(builtin("worker").unwrap(), &enabled, &ext).contains(&"write".to_owned()));
}
