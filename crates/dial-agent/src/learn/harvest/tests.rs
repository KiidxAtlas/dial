use super::*;
use crate::config::types::Outcome;
use std::path::PathBuf;

const CALL_FAILURE: &str = "That looked like a tool call written as plain text, so nothing ran. Call the tool through the tool-calling interface, or answer in words.";
const DROPPED: &str =
    "Your tool call was unreadable JSON. Issue it again with valid arguments, or answer directly.";

fn system() -> Value {
    json!({"role":"system","content":"sys"})
}
fn user(text: &str) -> Value {
    json!({"role":"user","content":text})
}
fn say(text: &str) -> Value {
    json!({"role":"assistant","content":text})
}
fn call(id: &str, path: &str) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"read","arguments":format!("{{\"path\":\"{path}\"}}")}}]})
}
fn answer(id: &str, text: &str) -> Value {
    json!({"role":"tool","tool_call_id":id,"content":text})
}
fn schemas() -> Value {
    json!([{"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}}])
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            &dir.path().join("state"),
            "model",
            "case",
            &Default::default(),
        )
        .unwrap();
        let mut config = Config::default();
        config.training.train_data = dir.path().join("data/train.jsonl");
        config.training.valid_data = dir.path().join("data/valid.jsonl");
        config.training.batch_size = 1;
        config.training.max_seq_length = 4096;
        Self { dir, store, config }
    }

    fn record(&self, domain: &str, score: f64, messages: Vec<Value>) -> i64 {
        let version = self.store.active().unwrap().id;
        let id = self
            .store
            .begin_trajectory(version, domain, "task", "prompt")
            .unwrap();
        self.store
            .finish_trajectory(id, Some("done"), None)
            .unwrap();
        self.store
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: "verifier:test".into(),
                reference: None,
            })
            .unwrap();
        self.store
            .save_messages(id, &json!({"messages":messages,"tools":schemas()}))
            .unwrap();
        id
    }

    fn run(&self) -> Report {
        self.run_with(&HashSet::new())
    }
    fn run_with(&self, held_out: &HashSet<String>) -> Report {
        harvest(&self.config, &self.store, held_out).unwrap()
    }

    /// All harvested records, train then valid.
    fn records(&self) -> Vec<Value> {
        let t = &self.config.training;
        [&t.train_data, &t.valid_data]
            .iter()
            .flat_map(|p: &&PathBuf| {
                fs::read_to_string(p)
                    .unwrap_or_default()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<Value>>()
            })
            .collect()
    }
}

fn roles(record: &Value) -> Vec<&str> {
    record["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect()
}

#[test]
fn arguments_become_objects_and_bad_json_is_rejected() {
    let payload = json!({"context":[{"role":"system","content":"s"},{"role":"user","content":"u"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},
        {"role":"tool","tool_call_id":"1","content":"x"}],
        "corrected":{"role":"assistant","content":null,"tool_calls":[{"id":"2","type":"function","function":{"name":"read","arguments":"{\"path\":\"b\"}"}}]},
        "tools":[]});
    let record = build(&payload, 7).unwrap();
    assert_eq!(
        record["messages"][4]["tool_calls"][0]["function"]["arguments"],
        json!({"path":"b"})
    );
    assert_eq!(
        record["messages"][2]["tool_calls"][0]["function"]["arguments"],
        json!({"path":"a"})
    );
    assert_eq!(record["source"], "dial:tool-recovery:7");
    let mut bad = payload.clone();
    bad["corrected"]["tool_calls"][0]["function"]["arguments"] = json!("not json");
    assert!(build(&bad, 7).is_none());
}

#[test]
fn harness_metadata_is_removed_from_successes_and_repairs_without_relaxing_validation() {
    let f = Fixture::new();
    let mut observed = answer("1", "evidence");
    observed["dial_evidence"] = json!({"artifact":"artifact://proof"});
    observed["is_error"] = json!(false);
    f.record(
        "live",
        100.0,
        vec![
            system(),
            user("inspect metadata"),
            call("1", "a"),
            observed.clone(),
            say("done"),
        ],
    );
    let report = f.run();
    assert_eq!(report.from_successes, 2);
    let records = f.records();
    assert!(
        records
            .iter()
            .all(|r| !r.to_string().contains("dial_evidence"))
    );
    let payload = json!({"context":[system(),user("repair"),call("1","a"),observed],
        "corrected":call("2","b"),"tools":schemas()});
    let record = build(&payload, 9).unwrap();
    assert!(
        record["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(well_formed)
    );
    assert_eq!(record["messages"][3]["content"], "evidence");
    assert!(record["messages"][3].get("is_error").is_none());
    let mut invalid_flag = answer("1", "evidence");
    invalid_flag["is_error"] = json!("false");
    assert!(!well_formed(&objectify(&invalid_flag).unwrap()));
    let mut unknown = answer("1", "evidence");
    unknown["unknown"] = json!(true);
    assert!(!well_formed(&objectify(&unknown).unwrap()));
    let mut image = answer("1", "evidence");
    image["dial_images"] = json!([{"path":"image.png"}]);
    assert!(!well_formed(&objectify(&image).unwrap()));
}

#[test]
fn request_snapshots_preserve_context_and_never_relabel_previous_turns() {
    let f = Fixture::new();
    let id = f.record(
        "live",
        100.0,
        vec![system(), user("placeholder"), say("legacy")],
    );
    let messages = vec![
        system(),
        user("original constraints"),
        say("old answer"),
        user("continue"),
        call("1", "a"),
        answer("1", "actual evidence"),
        say("new answer"),
    ];
    f.store
        .save_messages(
            id,
            &json!({"examples":[{"messages":messages,"tools":schemas()}]}),
        )
        .unwrap();
    assert_eq!(f.run().from_successes, 1);
    let records = f.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["messages"][2]["content"], "old answer");
    assert_eq!(records[0]["messages"][6]["content"], "new answer");
    assert_eq!(
        records[0]["messages"][4]["tool_calls"][0]["function"]["arguments"],
        json!({"path":"a"})
    );
    assert_eq!(f.run().added(), 0);
}

#[test]
fn request_snapshots_keep_heldout_and_size_checks() {
    let f = Fixture::new();
    let id = f.record(
        "live",
        100.0,
        vec![system(), user("placeholder"), say("legacy")],
    );
    f.store.save_messages(id, &json!({"examples":[
        {"messages":[system(),user("sealed task"),say("old"),user("continue"),say("new")],"tools":schemas()},
        {"messages":[system(),user("large"),say(&"x".repeat(10000))],"tools":schemas()},
        {"messages":[system(),user("bad"),{"role":"assistant","content":"new","unknown":true}],"tools":schemas()}
    ]})).unwrap();
    let report = f.run_with(&HashSet::from(["sealed task".into()]));
    assert_eq!(report.added(), 0);
    assert_eq!(report.skipped_overlap, 1);
    assert_eq!(report.skipped_oversize, 1);
    assert_eq!(report.skipped_invalid, 1);
}

#[test]
fn clean_success_yields_one_record_per_assistant_turn_with_loss_last() {
    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("read a"),
            call("c1", "a"),
            answer("c1", "text"),
            say("it says text"),
        ],
    );
    let report = f.run();
    assert_eq!(
        (report.from_successes, report.from_repairs, report.added()),
        (2, 0, 2)
    );
    let mut records = f.records();
    records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
    assert_eq!(roles(&records[0]), ["system", "user", "assistant"]);
    assert_eq!(
        roles(&records[1]),
        ["system", "user", "assistant", "tool", "assistant"]
    );
    assert_eq!(
        records[0]["messages"][2]["tool_calls"][0]["function"]["arguments"],
        json!({"path":"a"})
    );
    assert_eq!(records[1]["messages"][4]["content"], "it says text");
    assert_eq!(
        records[1]["source"],
        format!("dial:verified:{id}:verifier:test")
    );
    assert_eq!(records[1]["verified"], true);
    assert_eq!(records[1]["tools"], schemas());
}

#[test]
fn failed_outcomes_are_not_learned_from() {
    let f = Fixture::new();
    f.record("selfplay:files", 99.0, vec![system(), user("q"), say("a")]);
    assert_eq!(f.run().added(), 0);
}

fn think(mut message: Value, reasoning: &str) -> Value {
    message["reasoning_content"] = json!(reasoning);
    message
}

#[test]
fn reasoning_is_kept_only_on_the_final_assistant_target() {
    let f = Fixture::new();
    let messages = vec![
        system(),
        user("read with thought"),
        think(call("c1", "a"), "plan the read"),
        answer("c1", "text"),
        think(say("it says text"), "summarize"),
    ];
    f.record("selfplay:files", 100.0, messages.clone());
    let report = f.run();
    // These targets used to be discarded as `skipped_invalid`.
    assert_eq!((report.skipped_invalid, report.from_successes), (0, 2));
    let mut records = f.records();
    records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
    assert_eq!(
        records[0]["messages"][2]["reasoning_content"],
        "plan the read"
    );
    assert!(records[1]["messages"][2].get("reasoning_content").is_none());
    assert_eq!(records[1]["messages"][4]["reasoning_content"], "summarize");
    // Request snapshots follow the same rule; blank reasoning stays malformed.
    let id = f.record(
        "live",
        100.0,
        vec![system(), user("placeholder"), say("legacy")],
    );
    let mut snapshot = messages;
    snapshot[1] = user("snapshot with thought");
    let mut blank = snapshot.clone();
    blank[4] = think(say("x"), " ");
    f.store
        .save_messages(
            id,
            &json!({"examples":[{"messages":snapshot,"tools":schemas()},{"messages":blank,"tools":schemas()}]}),
        )
        .unwrap();
    let report = f.run();
    assert_eq!((report.skipped_invalid, report.from_successes), (1, 1));
    let record = f
        .records()
        .into_iter()
        .find(|r| r["messages"][1]["content"] == "snapshot with thought")
        .unwrap();
    assert!(record["messages"][2].get("reasoning_content").is_none());
    assert_eq!(record["messages"][4]["reasoning_content"], "summarize");
}

#[test]
fn recoveries_keep_target_reasoning_but_pairs_strip_it() {
    let mut payload = recovery_payload("think repair", Some(vec![think(say("bad"), "wrong")]));
    payload["context"] = json!([
        system(),
        user("think repair"),
        think(say("earlier"), "old thought"),
        user("again")
    ]);
    payload["corrected"] = think(call("chosen", "a"), "fixed thought");
    let record = build(&payload, 1).unwrap();
    assert!(record["messages"][2].get("reasoning_content").is_none());
    assert_eq!(record["messages"][4]["reasoning_content"], "fixed thought");
    let pairs = verified_preference_pairs(&payload, &record);
    assert_eq!(pairs.len(), 1);
    assert!(!pairs[0].to_string().contains("reasoning_content"));
    assert_eq!(pairs[0]["chosen"], objectify(&call("chosen", "a")).unwrap());
}

#[test]
fn form_failures_and_their_corrections_are_removed_from_later_contexts() {
    let f = Fixture::new();
    let text_call = say("{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}");
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("read a"),
            text_call.clone(),
            user(CALL_FAILURE),
            user(DROPPED),
            call("c1", "a"),
            answer("c1", "text"),
            say("done"),
        ],
    );
    let report = f.run();
    assert_eq!(report.skipped_form_failure_turns, 1);
    assert_eq!(report.added(), 2);
    for record in f.records() {
        let text = record.to_string();
        assert!(
            !text.contains("plain text")
                && !text.contains("unreadable")
                && !text.contains("\\\"arguments\\\""),
            "{text}"
        );
        assert_eq!(record["messages"][2]["role"], "assistant");
        assert_eq!(record["messages"][2]["tool_calls"][0]["id"], "c1");
    }
}

#[test]
fn verified_json_answers_are_not_mistaken_for_failed_tool_calls() {
    let f = Fixture::new();
    let answer = "{\"arguments\": {\"count\": 3}}";
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("Return JSON describing the arguments"),
            say(answer),
        ],
    );
    let report = f.run();
    assert_eq!((report.skipped_form_failure_turns, report.added()), (0, 1));
    assert_eq!(f.records()[0]["messages"][2]["content"], answer);
}

#[test]
fn denied_and_invalid_calls_drop_their_turn_but_runtime_errors_are_kept() {
    let f = Fixture::new();
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("go"),
            call("c1", "x"),
            answer("c1", "denied: read-only mode; this tool changes things."),
            call("c2", "y"),
            answer("c2", "invalid tool call: missing required argument path"),
            call("c3", "z"),
            answer("c3", "reading \"z\": no such file or directory"),
            call("c4", "a"),
            answer("c4", "fine"),
            say("done"),
        ],
    );
    let report = f.run();
    assert_eq!(report.skipped_form_failure_turns, 2);
    let mut records = f.records();
    records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
    assert_eq!(records.len(), 3);
    // c3 (recovered runtime error) -> c4 -> final answer, with the rejected calls gone from every context.
    assert_eq!(roles(&records[0]), ["system", "user", "assistant"]);
    assert_eq!(records[0]["messages"][2]["tool_calls"][0]["id"], "c3");
    assert_eq!(
        roles(&records[2]),
        [
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "assistant"
        ]
    );
    for record in &records {
        let text = record.to_string();
        assert!(
            !text.contains("denied") && !text.contains("invalid tool call"),
            "{text}"
        );
    }
}

#[test]
fn session_domains_need_learn_from_sessions() {
    let mut f = Fixture::new();
    f.record("coding", 100.0, vec![system(), user("live"), say("answer")]);
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("synthetic"), say("answer")],
    );
    f.config.improve.learn_from_sessions = false;
    assert_eq!(f.run().added(), 1);
    assert!(
        f.records()
            .iter()
            .all(|r| r["messages"][1]["content"] == "synthetic")
    );
    f.config.improve.learn_from_sessions = true;
    assert_eq!(f.run().added(), 1);
    assert_eq!(f.records().len(), 2);
}

#[test]
fn disabling_live_learning_also_excludes_tool_repairs() {
    let mut f = Fixture::new();
    let id = f.record(
        "coding",
        0.0,
        vec![system(), user("private live task"), say("failed")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &json!({
                "context":[system(), user("private live task")], "tools":schemas(),
                "corrected":call("fixed", "a"), "failure_kinds":["schema"]
            }),
        )
        .unwrap();
    f.config.improve.learn_from_sessions = false;
    assert_eq!(f.run().added(), 0);
    f.config.improve.learn_from_sessions = true;
    assert_eq!(f.run().from_repairs, 1);
    assert_eq!(
        f.records()[0]["messages"][1]["content"],
        "private live task"
    );
}

#[test]
fn held_out_prompts_are_never_trained_on() {
    let f = Fixture::new();
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("  secret exam question \n"),
            call("c1", "a"),
            answer("c1", "x"),
            say("done"),
        ],
    );
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("ordinary"), say("fine")],
    );
    let report = f.run_with(&HashSet::from(["secret exam question".to_owned()]));
    assert_eq!((report.skipped_overlap, report.added()), (2, 1));
    assert!(
        f.records()
            .iter()
            .all(|r| !r.to_string().contains("secret exam"))
    );
}

#[test]
fn oversize_records_are_skipped_not_truncated() {
    let mut f = Fixture::new();
    f.config.training.max_seq_length = 600;
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("short"), say("ok")],
    );
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("long"), say(&"w ".repeat(2000))],
    );
    let report = f.run();
    assert_eq!((report.skipped_oversize, report.added()), (1, 1));
}

#[test]
fn trajectories_are_capped_keeping_the_final_answer() {
    let f = Fixture::new();
    let mut messages = vec![system(), user("many steps")];
    for i in 0..20 {
        messages.push(call(&format!("c{i}"), "a"));
        messages.push(answer(&format!("c{i}"), "ok"));
    }
    messages.push(say("the end"));
    f.record("selfplay:files", 100.0, messages);
    assert_eq!(f.run().added(), PER_TRAJECTORY);
    let records = f.records();
    assert!(
        records
            .iter()
            .any(|r| r["messages"].as_array().unwrap().last().unwrap()["content"] == "the end")
    );
    assert_eq!(
        records
            .iter()
            .map(|r| r["messages"].to_string())
            .collect::<HashSet<_>>()
            .len(),
        PER_TRAJECTORY
    );
}

#[test]
fn one_family_cannot_dominate_and_reruns_add_nothing() {
    let f = Fixture::new();
    let mut big = HashSet::new();
    for i in 0..6 {
        big.insert(f.record(
            "selfplay:big",
            100.0,
            vec![system(), user(&format!("big {i}")), say("a")],
        ));
    }
    for family in ["b", "c", "d"] {
        for i in 0..3 {
            f.record(
                &format!("selfplay:{family}"),
                100.0,
                vec![system(), user(&format!("{family} {i}")), say("a")],
            );
        }
    }
    let first = f.run();
    // Largest common size c with c <= 35% of sum(min(size, c)): 4 of 13.
    assert_eq!(first.added(), 13);
    let from_big = f
        .records()
        .iter()
        .filter(|r| {
            let source = r["source"].as_str().unwrap();
            big.iter()
                .any(|id| source.starts_with(&format!("dial:verified:{id}:")))
        })
        .count();
    assert_eq!(from_big, 4);
    let (train, valid) = (
        fs::read_to_string(&f.config.training.train_data).unwrap(),
        fs::read_to_string(&f.config.training.valid_data).unwrap(),
    );
    let second = f.run();
    assert_eq!(second.added(), 0);
    assert_eq!(
        (second.total_train, second.total_valid),
        (first.total_train, first.total_valid)
    );
    assert_eq!(
        fs::read_to_string(&f.config.training.train_data).unwrap(),
        train
    );
    assert_eq!(
        fs::read_to_string(&f.config.training.valid_data).unwrap(),
        valid
    );
    assert_eq!(
        f.store.get_meta("harvested_total").unwrap().as_deref(),
        Some("13")
    );
}

fn recovery_payload(prompt: &str, rejected: Option<Vec<Value>>) -> Value {
    let mut payload = json!({
        "context":[system(), user(prompt)],
        "tools":schemas(), "corrected":call("chosen", "a"), "failure_kinds":["schema"]
    });
    if let Some(rejected) = rejected {
        payload["rejected"] = json!(rejected);
    }
    payload
}

#[test]
fn verified_repair_pair_is_written_only_to_its_train_split() {
    let f = Fixture::new();
    let prompt = (0..100)
        .map(|i| format!("pair prompt {i}"))
        .find(|p| {
            let group = prompt_group(&json!([user(p)])).unwrap();
            !u8::from_str_radix(&group[..2], 16)
                .unwrap()
                .is_multiple_of(5)
        })
        .unwrap();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user(&prompt), say("done")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &recovery_payload(&prompt, Some(vec![say("malformed attempt")])),
        )
        .unwrap();
    f.run();
    let train: Vec<Value> = fs::read_to_string(&f.config.training.train_data)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let valid = fs::read_to_string(&f.config.training.valid_data).unwrap_or_default();
    let recovery = train
        .iter()
        .find(|r| r["source"] == format!("dial:tool-recovery:{id}"))
        .unwrap();
    assert_eq!(
        recovery["preference_pairs"][0]["chosen"],
        objectify(&call("chosen", "a")).unwrap()
    );
    assert_eq!(
        recovery["preference_pairs"][0]["rejected"],
        say("malformed attempt")
    );
    assert!(!valid.contains("preference_pairs"));
}

#[test]
fn missing_rejected_attempt_and_held_out_recovery_produce_no_pairs() {
    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("unpaired"), say("done")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &recovery_payload("unpaired", None),
        )
        .unwrap();
    f.run();
    assert!(
        f.records()
            .iter()
            .filter(|r| r["source"] == format!("dial:tool-recovery:{id}"))
            .all(|r| r.get("preference_pairs").is_none())
    );

    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("sealed"), say("done")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &recovery_payload("sealed", Some(vec![say("malformed attempt")])),
        )
        .unwrap();
    f.run_with(&HashSet::from(["sealed".to_owned()]));
    assert!(
        !f.records()
            .iter()
            .any(|r| r["source"] == format!("dial:tool-recovery:{id}"))
    );
}

#[test]
fn malformed_context_or_rejected_output_produces_no_pairs() {
    let mut payload = recovery_payload("overlap", Some(vec![say("bad")]));
    payload["context"] = json!([system(), user("overlap"), {"role":"assistant","content":""}]);
    let record = build(&payload, 1).unwrap();
    assert!(record.get("preference_pairs").is_none());

    let payload = recovery_payload(
        "invalid",
        Some(vec![json!({"role":"assistant","tool_calls":[]} )]),
    );
    let record = build(&payload, 2).unwrap();
    assert!(record.get("preference_pairs").is_none());
}

#[test]
fn overlapping_recovery_does_not_attach_a_preference_pair() {
    let f = Fixture::new();
    let prompt = "already trained repair";
    let recovery_record = render(
        vec![
            system(),
            user(prompt),
            objectify(&call("chosen", "a")).unwrap(),
        ],
        &schemas(),
        "existing".into(),
    );
    fs::create_dir_all(f.config.training.train_data.parent().unwrap()).unwrap();
    fs::write(
        &f.config.training.train_data,
        format!("{recovery_record}\n"),
    )
    .unwrap();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user(prompt), say("done")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &recovery_payload(prompt, Some(vec![say("malformed attempt")])),
        )
        .unwrap();
    f.run();
    assert!(
        f.records()
            .iter()
            .all(|record| record.get("preference_pairs").is_none())
    );
}
#[test]
fn repairs_and_successes_share_a_clean_deduplicated_pipeline() {
    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user("read a"),
            call("bad", "missing"),
            answer("bad", "invalid tool call: missing path"),
            call("c2", "a"),
            answer("c2", "text"),
            say("done"),
        ],
    );
    let version = f.store.active().unwrap().id;
    f.store
        .tool_recovery(
            id,
            version,
            &json!({
                "context":[system(), user("read a"), call("bad", "missing"),
                    answer("bad", "invalid tool call: missing path")],
                "tools":schemas(), "corrected":call("c2", "a"), "failure_kinds":["schema"]
            }),
        )
        .unwrap();
    let report = f.run();
    assert_eq!(
        (report.from_repairs, report.from_successes, report.added()),
        (1, 1, 2)
    );
    let mut records = f.records();
    records.sort_by_key(|r| r["messages"].as_array().unwrap().len());
    assert_eq!(
        records[0]["messages"],
        json!([
            system(),
            user("read a"),
            objectify(&call("c2", "a")).unwrap()
        ])
    );
    assert_eq!(records[1]["messages"][4]["content"], "done");
    assert_eq!(f.run().added(), 0);
}

#[test]
fn command_exit_codes_do_not_make_successful_training_targets() {
    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("repair"), say("claimed done")],
    );
    f.store
        .outcome(&Outcome {
            trajectory: id,
            score: 100.0,
            source: "check:exit0".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(f.run().added(), 0);
    f.store
        .outcome(&Outcome {
            trajectory: id,
            score: 100.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(f.run().added(), 1);
}

#[test]
fn prefixes_and_later_same_prompt_trajectories_keep_their_split() {
    let f = Fixture::new();
    // Pick an ordinary training group, then verify that the validation floor cannot split it.
    let prompt = (0..100)
        .map(|i| format!("read file {i}"))
        .find(|p| {
            let group = prompt_group(&json!([user(p)])).unwrap();
            !u8::from_str_radix(&group[..2], 16)
                .unwrap()
                .is_multiple_of(5)
        })
        .unwrap();
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user(&prompt),
            call("c1", "a"),
            answer("c1", "x"),
            say("x"),
        ],
    );
    let first = f.run();
    assert_eq!((first.added_train, first.added_valid), (2, 0));
    f.record(
        "selfplay:files",
        100.0,
        vec![
            system(),
            user(&prompt),
            call("c1", "a"),
            answer("c1", "x"),
            say("The answer is x"),
        ],
    );
    let next = f.run();
    assert_eq!((next.added_train, next.added_valid), (1, 0));
    // An independent prompt can supply validation without moving the existing group.
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("a different task"), say("other")],
    );
    let last = f.run();
    assert_eq!((last.added_train, last.added_valid), (0, 1));
    let (_, _, train_groups) = digests(&f.config.training.train_data).unwrap();
    let (_, _, valid_groups) = digests(&f.config.training.valid_data).unwrap();
    assert!(train_groups.is_disjoint(&valid_groups));
    assert_eq!(f.run().added(), 0);
}

#[test]
fn compacted_recovery_and_success_are_one_split_group() {
    let f = Fixture::new();
    let id = f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("recent context"), say("done")],
    );
    f.store
        .tool_recovery(
            id,
            f.store.active().unwrap().id,
            &json!({
                "context":[system(), user("original context")],
                "tools":schemas(), "corrected":call("c1", "a"), "failure_kinds":["schema"]
            }),
        )
        .unwrap();
    // A different trajectory with the same original prompt joins the component too.
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("original context"), say("also done")],
    );
    let report = f.run();
    assert!(matches!(
        (report.added_train, report.added_valid),
        (3, 0) | (0, 3)
    ));
    assert_eq!(f.run().added(), 0);
}

#[test]
fn small_corpus_keeps_both_splits_without_splitting_a_group() {
    let f = Fixture::new();
    let prompts: Vec<_> = (0..100)
        .map(|i| format!("question {i}"))
        .filter(|p| {
            let group = prompt_group(&json!([user(p)])).unwrap();
            u8::from_str_radix(&group[..2], 16)
                .unwrap()
                .is_multiple_of(5)
        })
        .take(2)
        .collect();
    assert_eq!(prompts.len(), 2);
    for prompt in prompts {
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user(&prompt), say("answer")],
        );
    }
    let report = f.run();
    assert_eq!((report.added_train, report.added_valid), (1, 1));
    assert_eq!(f.run().added(), 0);
}

#[test]
fn preexisting_prompt_overlap_is_rejected_even_with_distinct_answers() {
    let f = Fixture::new();
    fs::create_dir_all(f.dir.path().join("data")).unwrap();
    for (path, answer) in [
        (&f.config.training.train_data, "yes"),
        (&f.config.training.valid_data, "certainly"),
    ] {
        fs::write(
            path,
            format!(
                "{}\n",
                render(
                    vec![user("same question"), say(answer)],
                    &json!([]),
                    "operator".into()
                )
            ),
        )
        .unwrap();
    }
    let error = harvest(&f.config, &f.store, &HashSet::new()).unwrap_err();
    assert!(error.to_string().contains("prompts overlap"));
}

#[test]
fn appending_preserves_jsonl_without_an_existing_trailing_newline() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    fs::write(&path, "{\"a\":1}").unwrap();
    append(&path, "{\"a\":2}\n").unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "{\"a\":1}\n{\"a\":2}\n");
}

#[test]
fn torn_final_record_is_skipped_and_truncated_on_append() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    fs::write(&path, "{\"a\":1}\n{\"messages\":[{\"role\":\"user\"").unwrap();
    let (_, count, _) = digests(&path).unwrap();
    assert_eq!(count, 1);
    append(&path, "{\"a\":2}\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"a\":2}\n");
    assert_eq!(digests(&path).unwrap().1, 2);
}

#[test]
fn corrupt_record_before_the_end_is_still_an_error() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    fs::write(&path, "{bad\n{\"a\":1}\n").unwrap();
    assert!(digests(&path).is_err());
}

#[test]
fn torn_multibyte_tail_is_recovered_at_every_byte_cut() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    let tail = "{\"messages\":\"é日🎉\"}".as_bytes();
    let start = tail.iter().position(|&b| b == b'"').unwrap() + 1;
    // Cut after every byte, including inside 2-, 3- and 4-byte characters.
    for cut in start..tail.len() {
        let mut bytes = b"{\"a\":1}\n".to_vec();
        bytes.extend_from_slice(&tail[..cut]);
        fs::write(&path, &bytes).unwrap();
        let (_, count, _) = digests(&path).unwrap();
        assert_eq!(count, 1, "cut at byte {cut}");
        append(&path, "{\"a\":2}\n").unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"{\"a\":1}\n{\"a\":2}\n",
            "cut at byte {cut}"
        );
        assert_eq!(digests(&path).unwrap().1, 2, "cut at byte {cut}");
    }
}

#[test]
fn complete_multibyte_record_missing_only_its_newline_is_kept() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    fs::write(&path, "{\"a\":1}\n{\"a\":\"é日🎉\"}").unwrap();
    assert_eq!(digests(&path).unwrap().1, 2);
    append(&path, "{\"a\":3}\n").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\"a\":1}\n{\"a\":\"é日🎉\"}\n{\"a\":3}\n"
    );
}

#[test]
fn invalid_utf8_interior_or_complete_record_is_rejected_without_rewriting() {
    let f = Fixture::new();
    let path = f.dir.path().join("data.jsonl");
    let interior = b"{\"a\":\"\xff\"}\n{\"a\":1}\n".to_vec();
    fs::write(&path, &interior).unwrap();
    assert!(digests(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), interior);
    let complete_tail = b"{\"a\":1}\n{\"a\":\"\xff\"}".to_vec();
    fs::write(&path, &complete_tail).unwrap();
    // A malformed unterminated record is torn: it is skipped, never a reason to fail.
    assert_eq!(digests(&path).unwrap().1, 1);
}

/// A sibling model database beside the fixture's own; its directory name is the origin.
fn sibling(f: &Fixture, name: &str) -> Store {
    Store::open(
        &f.dir.path().join(name),
        "model-b",
        "case",
        &Default::default(),
    )
    .unwrap()
}

fn record_on(store: &Store, domain: &str, score: f64, messages: Vec<Value>) {
    let version = store.active().unwrap().id;
    let id = store
        .begin_trajectory(version, domain, "task", "prompt")
        .unwrap();
    store.finish_trajectory(id, Some("done"), None).unwrap();
    store
        .outcome(&Outcome {
            trajectory: id,
            score,
            source: "verifier:test".into(),
            reference: None,
        })
        .unwrap();
    store
        .save_messages(id, &json!({"messages":messages,"tools":schemas()}))
        .unwrap();
}

#[test]
fn learned_records_need_a_named_sibling_and_carry_provenance() {
    let mut f = Fixture::new();
    f.config.state_dir = f.dir.path().join("state");
    f.config.training.learn_from_max_percent = 50;
    record_on(
        &sibling(&f, "aaaa"),
        "selfplay:files",
        100.0,
        vec![system(), user("sibling task"), say("done")],
    );
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("local task"), say("done")],
    );
    assert_eq!(f.run().from_other_models, 0);
    f.config.training.learn_from = vec!["model-c".into()];
    assert_eq!(f.run().from_other_models, 0);
    // A `provider/` prefix matches the model name recorded in the sibling database.
    f.config.training.learn_from = vec!["provider/model-b".into()];
    let report = f.run();
    assert_eq!(report.from_other_models, 1);
    assert_eq!(report.learned_from.get("aaaa"), Some(&1));
    assert!(f.records().iter().any(|r| {
        r["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("dial:learned:aaaa:"))
    }));
}

#[test]
fn learned_records_skip_unverified_and_held_out_trajectories() {
    let mut f = Fixture::new();
    f.config.state_dir = f.dir.path().join("state");
    f.config.training.learn_from_all_verified = true;
    f.config.training.learn_from_max_percent = 50;
    f.record(
        "selfplay:files",
        100.0,
        vec![system(), user("local task"), say("done")],
    );
    let other = sibling(&f, "bbbb");
    record_on(
        &other,
        "selfplay:files",
        40.0,
        vec![system(), user("unverified task"), say("done")],
    );
    record_on(
        &other,
        "selfplay:files",
        100.0,
        vec![system(), user("sealed task"), say("done")],
    );
    record_on(
        &other,
        "selfplay:files",
        100.0,
        vec![system(), user("open task"), say("done")],
    );
    let report = f.run_with(&HashSet::from(["sealed task".to_owned()]));
    assert_eq!((report.from_other_models, report.skipped_overlap), (1, 1));
    let text = serde_json::to_string(&f.records()).unwrap();
    assert!(text.contains("open task"));
    assert!(!text.contains("unverified task") && !text.contains("sealed task"));
}

#[test]
fn learned_records_fill_a_thin_local_batch_up_to_the_minimum() {
    let mut f = Fixture::new();
    f.config.state_dir = f.dir.path().join("state");
    f.config.training.learn_from_all_verified = true;
    f.config.training.learn_from_max_percent = 25;
    f.config.improve.lora_min_examples = 8;
    for i in 0..3 {
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user(&format!("local {i}")), say("done")],
        );
    }
    let other = sibling(&f, "cccc");
    for i in 0..12 {
        record_on(
            &other,
            "selfplay:files",
            100.0,
            vec![system(), user(&format!("remote {i}")), say("done")],
        );
    }
    let report = f.run();
    // Below the minimum of 8, foreign records fill the batch to 8 (8 - 3 = 5). The 25% share
    // alone would admit only 3 * 25 / 75 = 1, so the minimum is what admits the five.
    assert_eq!((report.from_other_models, report.skipped_learn_cap), (5, 7));
    assert_eq!(report.from_successes, 8);
}

#[test]
fn learned_share_is_capped_by_local_records_at_or_above_the_minimum() {
    let mut f = Fixture::new();
    f.config.state_dir = f.dir.path().join("state");
    f.config.training.learn_from_all_verified = true;
    f.config.training.learn_from_max_percent = 25;
    f.config.improve.lora_min_examples = 8;
    for i in 0..12 {
        f.record(
            "selfplay:files",
            100.0,
            vec![system(), user(&format!("local {i}")), say("done")],
        );
    }
    let other = sibling(&f, "cccc");
    for i in 0..12 {
        record_on(
            &other,
            "selfplay:files",
            100.0,
            vec![system(), user(&format!("remote {i}")), say("done")],
        );
    }
    let report = f.run();
    // At 12 local records the share is 12 * 25 / 75 = 4; the minimum (8) no longer applies.
    assert_eq!((report.from_other_models, report.skipped_learn_cap), (4, 8));
    assert_eq!(report.from_successes, 16);
}

#[test]
fn foreign_repairs_keep_their_origin_in_the_source() {
    let payload = json!({
        "context": [system(), user("read a"), call("bad", "missing"),
            answer("bad", "invalid tool call: missing path")],
        "tools": schemas(),
        "corrected": call("c2", "a"),
        "failure_kinds": ["schema"]
    });
    let record = foreign_repair(&payload, "aaaa", 7).unwrap();
    assert_eq!(record["source"], "dial:learned:aaaa:7:tool-recovery");
    assert!(record.get("preference_pairs").is_none());
}

#[test]
fn trajectory_provenance_matches_shared_golden_cases() {
    let cases: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/trajectory_provenance.json"
    ))
    .unwrap();
    for case in cases.as_array().unwrap() {
        assert_eq!(
            trajectory_group(case["source"].as_str().unwrap()).as_deref(),
            case["group"].as_str(),
            "{}",
            case["source"]
        );
    }
}

#[test]
fn oversized_trajectory_ids_reject_overflow_but_accept_leading_zeroes() {
    assert_eq!(
        trajectory_group(&format!("dial:verified:{}", "9".repeat(5000))),
        None
    );
    assert_eq!(
        trajectory_group(&format!("dial:verified:{}7", "0".repeat(5000))).as_deref(),
        Some("trajectory:7")
    );
}

#[test]
fn sibling_trajectories_are_keyed_apart_from_local_ones() {
    assert_eq!(
        trajectory_group("dial:verified:7:verifier:test").as_deref(),
        Some("trajectory:7")
    );
    assert_eq!(
        trajectory_group("dial:learned:aaaa:7:verifier:test").as_deref(),
        Some("trajectory:aaaa:7")
    );
}

/// A coach-assigned teacher database holding one verified and one unverified run.
fn teacher_fixture(f: &mut Fixture, held_out_prompt: &str) -> Store {
    f.config.state_dir = f.dir.path().join("own");
    f.config.improve.distill = true;
    f.config.model_roles.insert(
        "coach".into(),
        crate::llm::model_config::ModelSelection::new("mlx", "coach-model"),
    );
    let teacher = Store::open(
        &super::super::distill::teacher_dir(&f.config),
        "coach-model",
        "case",
        &Default::default(),
    )
    .unwrap();
    let version = teacher.active().unwrap().id;
    let add = |prompt: &str, score: f64| {
        let id = teacher
            .begin_trajectory(version, "selfplay:read_line", "task", prompt)
            .unwrap();
        teacher.finish_trajectory(id, Some("done"), None).unwrap();
        teacher
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: "verifier:read_line".into(),
                reference: None,
            })
            .unwrap();
        teacher
            .save_messages(
                id,
                &json!({"messages":[system(), user(prompt), say("answer")],"tools":schemas()}),
            )
            .unwrap();
    };
    add("teach verified", 100.0);
    add("teach unverified", 0.0);
    add(held_out_prompt, 100.0);
    teacher
}

#[test]
fn verified_teacher_runs_are_tagged_and_unverified_or_sealed_ones_stay_out() {
    let mut f = Fixture::new();
    teacher_fixture(&mut f, "teach sealed");
    let held: HashSet<String> = ["teach sealed".to_owned()].into();
    let report = f.run_with(&held);
    assert_eq!(report.from_teacher, 1);
    let records = f.records();
    assert_eq!(records.len(), 1);
    let source = records[0]["source"].as_str().unwrap();
    assert!(
        source.starts_with("dial:learned:teacher-coach-model:"),
        "{source}"
    );
    assert!(records[0].to_string().contains("teach verified"));
    assert!(!records[0].to_string().contains("teach unverified"));
    assert!(!records[0].to_string().contains("teach sealed"));
}

#[test]
fn teacher_runs_are_ignored_unless_distillation_is_enabled() {
    let mut f = Fixture::new();
    teacher_fixture(&mut f, "teach sealed");
    f.config.improve.distill = false;
    let report = f.run();
    assert_eq!((report.from_teacher, report.added()), (0, 0));
}

fn approved_snapshot(turn: &str, payload: Value) -> crate::storage::store::SessionTrainingExample {
    crate::storage::store::SessionTrainingExample {
        version: 1,
        source_session: "source-session-a".into(),
        source_turn: turn.into(),
        source_model: "source-model-a".into(),
        source_state: None,
        source_trajectory: None,
        prompt: format!("source task {turn}"),
        payload,
    }
}

#[test]
fn explicit_session_imports_screen_snapshots_and_strip_foreign_reasoning() {
    let mut f = Fixture::new();
    // Explicit user approval is independent of automatic session/sibling learning.
    f.config.improve.learn_from_sessions = false;
    f.config.training.learn_from.clear();
    let payload = json!({"examples":[
        {"messages":[system(),user("sealed import"),say("no")],"tools":schemas()},
        {"messages":[system(),user("large import"),say(&"x".repeat(10000))],"tools":schemas()},
        {"messages":[system(),user("invalid import"),{"role":"assistant","content":"no","unknown":true}],"tools":schemas()},
        {"messages":[system(),user("retained constraints"),think(say("previous answer"),"private prior reasoning"),user("continue import"),think(say("new answer"),"target reasoning")],"tools":schemas()}
    ]});
    f.store
        .approve_session_examples(&[approved_snapshot("screened", payload)])
        .unwrap();
    let report = f.run_with(&HashSet::from(["sealed import".into()]));
    assert_eq!(
        (
            report.skipped_overlap,
            report.skipped_oversize,
            report.skipped_invalid
        ),
        (1, 1, 1)
    );
    assert_eq!(report.added(), 1);
    let records = f.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        roles(&records[0]),
        ["system", "user", "assistant", "user", "assistant"]
    );
    assert_eq!(records[0]["messages"][2]["content"], "previous answer");
    assert!(records[0]["messages"][2].get("reasoning_content").is_none());
    assert!(records[0]["messages"][4].get("reasoning_content").is_none());
    assert_eq!(
        f.run_with(&HashSet::from(["sealed import".into()])).added(),
        0
    );
}

#[test]
fn explicit_session_corpus_is_not_capped_to_the_training_minimum_or_sibling_share() {
    let mut f = Fixture::new();
    f.config.improve.learn_from_sessions = false;
    f.config.training.batch_size = 1;
    f.config.improve.lora_min_examples = 2;
    let examples: Vec<_> = (0..12).map(|i| {
        let mut example = approved_snapshot(&format!("turn-{i}"), json!({"examples":[{
            "messages":[system(), user(&format!("independent imported task {i}")), say(&format!("answer {i}"))],
            "tools":schemas()
        }]}));
        // Adjacent turns share a source session but have different post-compaction prompts.
        example.source_session = format!("source-session-{}", i / 2);
        example
    }).collect();
    f.store.approve_session_examples(&examples).unwrap();
    let report = f.run();
    assert_eq!(report.added(), examples.len());
    assert!(report.added_train >= 2);
    assert!(report.added_valid > 0);
    for path in [&f.config.training.train_data, &f.config.training.valid_data] {
        let prompts: HashSet<String> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                record["messages"][1]["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        for session in 0..6 {
            assert_eq!(
                prompts.contains(&format!("independent imported task {}", session * 2)),
                prompts.contains(&format!("independent imported task {}", session * 2 + 1)),
                "source session {session} must not cross splits at {}",
                path.display()
            );
        }
    }
    assert_eq!(f.store.verified_live_turns().unwrap(), 0);
    assert_eq!(
        f.store.live_score(f.store.active().unwrap().id).unwrap(),
        (0, None)
    );
    assert_eq!(f.run().added(), 0);
}

#[test]
fn imported_snapshot_rechecks_source_verdict_before_harvesting() {
    let mut f = Fixture::new();
    f.config.state_dir = f.dir.path().join("state");
    let source = sibling(&f, "source-a");
    let trajectory = source
        .begin_trajectory(
            source.active().unwrap().id,
            "coding",
            "task",
            "later rejected import",
        )
        .unwrap();
    source
        .finish_trajectory(trajectory, Some("done"), None)
        .unwrap();
    let mut retained = approved_snapshot(
        "later-rejected",
        json!({"examples":[{
            "messages":[system(),user("later rejected import"),say("not good enough")],"tools":schemas()
        }]}),
    );
    retained.source_state = Some(f.dir.path().join("source-a"));
    retained.source_trajectory = Some(trajectory);
    f.store.approve_session_examples(&[retained]).unwrap();
    source
        .outcome(&Outcome {
            trajectory,
            score: 40.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(f.run().added(), 0);
    assert!(f.records().is_empty());
    assert_eq!(source.trajectory_score(trajectory).unwrap(), Some(40.0));
    assert_eq!(f.store.session_training_examples().unwrap().len(), 1);

    // Export while approved, then reject that old turn and add a post-compaction turn.
    // Frozen exported records still anchor the entire source session to its original split.
    source
        .outcome(&Outcome {
            trajectory,
            score: 100.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    assert_eq!(f.run().added(), 1);
    let was_valid = fs::read_to_string(&f.config.training.valid_data)
        .unwrap_or_default()
        .lines()
        .count()
        == 1;
    source
        .outcome(&Outcome {
            trajectory,
            score: 40.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    let next_prompt = (0..1000)
        .map(|i| format!("post-compaction task {i}"))
        .find(|prompt| {
            u8::from_str_radix(&digest(&json!(prompt))[..2], 16)
                .unwrap()
                .is_multiple_of(5)
                != was_valid
        })
        .unwrap();
    f.store.approve_session_examples(&[approved_snapshot("next-turn", json!({"examples":[{
        "messages":[system(),user(&next_prompt),say("new successful work")],"tools":schemas()
    }]}))]).unwrap();
    assert_eq!(f.run().added(), 1);
    let (same, other) = if was_valid {
        (&f.config.training.valid_data, &f.config.training.train_data)
    } else {
        (&f.config.training.train_data, &f.config.training.valid_data)
    };
    assert_eq!(fs::read_to_string(same).unwrap().lines().count(), 2);
    assert!(fs::read_to_string(other).unwrap_or_default().is_empty());
}
