use super::*;
use std::cell::Cell;

/// Replies scripted in order; counts calls so tests can prove cache hits skip the model.
struct Scripted {
    replies: Vec<&'static str>,
    calls: Cell<usize>,
    users: std::cell::RefCell<Vec<String>>,
    cost: Option<f64>,
}

impl Scripted {
    fn new(replies: Vec<&'static str>) -> Self {
        Self {
            replies,
            calls: Cell::new(0),
            users: Default::default(),
            cost: None,
        }
    }
}

impl Backend for Scripted {
    fn answer(&mut self, question: &Question<'_>, retry: bool) -> Result<Answer> {
        let user = user_prompt(question, retry);
        let index = self.calls.get();
        self.calls.set(index + 1);
        self.users.borrow_mut().push(user);
        let text = self
            .replies
            .get(index)
            .copied()
            .ok_or_else(|| anyhow!("no scripted reply left"))?;
        Ok(Answer {
            text: text.to_owned(),
            cost_usd: self.cost,
        })
    }
}

struct Failing;
impl Backend for Failing {
    fn answer(&mut self, _: &Question<'_>, _: bool) -> Result<Answer> {
        bail!("provider unreachable")
    }
}

const CHOICES: [&str; 3] = ["yes", "no", "maybe"];

fn question() -> Question<'static> {
    Question {
        question: "Is it safe?",
        choices: &CHOICES,
        criteria: &[],
        context: "a change",
    }
}

#[test]
fn reply_must_be_json_naming_a_listed_choice() {
    assert_eq!(
        parse_choice(r#"{"choice":"yes"}"#, &CHOICES).unwrap(),
        "yes"
    );
    assert_eq!(
        parse_choice("  {\"choice\": \"no\"}\n", &CHOICES).unwrap(),
        "no"
    );
    for bad in [
        "yes",
        r#"{"choice":"YES"}"#,
        r#"{"choice":"unknown"}"#,
        r#"{"answer":"yes"}"#,
        r#"{"choice":3}"#,
        "```json\n{\"choice\":\"yes\"}\n```",
    ] {
        assert!(parse_choice(bad, &CHOICES).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn invalid_reply_is_retried_once_then_accepted() {
    let mut backend = Scripted::new(vec!["probably", r#"{"choice":"maybe"}"#]);
    let judgment = judge(None, &mut backend, "m", &question(), 0).unwrap();
    assert_eq!(judgment.choice, "maybe");
    assert!(!judgment.cached);
    assert_eq!(backend.calls.get(), 2);
    assert!(backend.users.borrow()[1].contains("previous_reply"));
}

#[test]
fn two_invalid_replies_fail_with_the_last_reason() {
    let mut backend = Scripted::new(vec!["x", "y"]);
    let error = judge(None, &mut backend, "m", &question(), 0).unwrap_err();
    assert_eq!(backend.calls.get(), 2);
    assert!(error.to_string().contains("not JSON"), "{error}");
}

#[test]
fn provider_errors_are_not_retried() {
    let error = judge(None, &mut Failing, "m", &question(), 0).unwrap_err();
    assert!(error.to_string().contains("unreachable"));
}

#[test]
fn malformed_questions_are_rejected_before_any_call() {
    let mut backend = Scripted::new(vec![]);
    let one: [&str; 1] = ["only"];
    let bad_questions = [
        Question {
            question: " ",
            choices: &CHOICES,
            criteria: &[],
            context: "",
        },
        Question {
            question: "q",
            choices: &one,
            criteria: &[],
            context: "",
        },
        Question {
            question: "q",
            choices: &["a", "a"],
            criteria: &[],
            context: "",
        },
        Question {
            question: "q",
            choices: &[" a", "b"],
            criteria: &[],
            context: "",
        },
        Question {
            question: "q",
            choices: &["a", "b"],
            criteria: &["only a"],
            context: "",
        },
    ];
    for q in bad_questions {
        assert!(judge(None, &mut backend, "m", &q, 0).is_err());
    }
    assert_eq!(backend.calls.get(), 0);
}

#[test]
fn cache_hit_skips_the_model_and_reports_cached() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache::Cache::open(&dir.path().join("j.db"), 100, 10).unwrap();
    let mut backend = Scripted::new(vec![r#"{"choice":"no"}"#]);
    let first = judge(Some(&cache), &mut backend, "m", &question(), 10).unwrap();
    let second = judge(Some(&cache), &mut backend, "m", &question(), 11).unwrap();
    assert_eq!(first.choice, "no");
    assert!(!first.cached);
    assert_eq!(second.choice, "no");
    assert!(second.cached);
    assert_eq!(second.cost_usd, None);
    assert_eq!(
        backend.calls.get(),
        1,
        "second call must not reach the model"
    );
}

#[test]
fn expired_entry_is_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache::Cache::open(&dir.path().join("j.db"), 100, 10).unwrap();
    let mut backend = Scripted::new(vec![r#"{"choice":"no"}"#, r#"{"choice":"yes"}"#]);
    judge(Some(&cache), &mut backend, "m", &question(), 0).unwrap();
    let later = judge(Some(&cache), &mut backend, "m", &question(), 1000).unwrap();
    assert_eq!(later.choice, "yes");
    assert!(!later.cached);
    assert_eq!(backend.calls.get(), 2);
}

#[test]
fn key_separates_model_and_every_input() {
    let base = key_for("m", &question());
    assert_eq!(base, key_for("m", &question()));
    assert_ne!(base, key_for("other", &question()));
    let context = Question {
        context: "different",
        ..question()
    };
    assert_ne!(base, key_for("m", &context));
    let question_text = Question {
        question: "Is it fast?",
        ..question()
    };
    assert_ne!(base, key_for("m", &question_text));
    let fewer: [&str; 2] = ["yes", "no"];
    assert_ne!(
        base,
        key_for(
            "m",
            &Question {
                choices: &fewer,
                ..question()
            }
        )
    );
    let described = Question {
        criteria: &["safe", "unsafe", "unsure"],
        ..question()
    };
    assert_ne!(base, key_for("m", &described));
    // Length prefixes stop ("ab","c") and ("a","bc") from colliding.
    let left = Question {
        question: "ab",
        context: "c",
        ..question()
    };
    let right = Question {
        question: "a",
        context: "bc",
        ..question()
    };
    assert_ne!(key_for("m", &left), key_for("m", &right));
}

#[test]
fn cost_accumulates_across_retries_and_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache::Cache::open(&dir.path().join("j.db"), 100, 10).unwrap();
    let mut backend = Scripted::new(vec!["bad", r#"{"choice":"yes"}"#]);
    backend.cost = Some(0.25);
    let fresh = judge(Some(&cache), &mut backend, "m", &question(), 0).unwrap();
    assert_eq!(fresh.cost_usd, Some(0.5));
    let hit = judge(Some(&cache), &mut backend, "m", &question(), 1).unwrap();
    assert_eq!(hit.cost_usd, None);
}

#[test]
fn effort_clamps_to_the_nearest_declared_level_below() {
    use ReasoningEffort::*;
    assert_eq!(clamp_effort(High, &[Low, Medium, High]), Some(High));
    assert_eq!(clamp_effort(High, &[Low, Medium]), Some(Medium));
    assert_eq!(clamp_effort(Medium, &[Xhigh, Low]), Some(Low));
    // Nothing declared at or below the desired level: keep the model default.
    assert_eq!(clamp_effort(Low, &[Medium, High]), Option::None);
    assert_eq!(clamp_effort(None, &[Low]), Option::None);
    assert_eq!(clamp_effort(None, &[None, Low]), Some(None));
    assert_eq!(clamp_effort(High, &[]), Option::None);
}

#[test]
fn thinking_choices_map_to_effort_and_reject_unknown_names() {
    assert_eq!(effort_for_choice("off"), Some(ReasoningEffort::None));
    assert_eq!(effort_for_choice("high"), Some(ReasoningEffort::High));
    assert_eq!(effort_for_choice("auto"), None);
    for choice in THINKING_CHOICES {
        assert!(effort_for_choice(choice).is_some(), "{choice}");
    }
}

#[test]
fn long_prompts_are_bounded_on_a_char_boundary() {
    let long = "é".repeat(MAX_CONTEXT_CHARS + 10);
    let cut = bounded(&long);
    assert_eq!(cut.chars().count(), MAX_CONTEXT_CHARS);
    assert_eq!(bounded("short"), "short");
}

#[test]
fn judge_config_prefers_judge_then_small_then_main() {
    use crate::llm::model_config::ModelSelection;
    let selection = |model: &str| ModelSelection {
        provider: "openai".into(),
        model: model.into(),
        ..Default::default()
    };
    let mut config = Config {
        model: "main-model".into(),
        ..Default::default()
    };
    assert_eq!(judge_config(&config).model, "main-model");
    config
        .model_roles
        .insert("small".into(), selection("small-model"));
    assert_eq!(judge_config(&config).model, "small-model");
    config
        .model_roles
        .insert("judge".into(), selection("judge-model"));
    assert_eq!(judge_config(&config).model, "judge-model");
}

#[test]
fn criteria_reach_the_prompt_and_default_to_the_choice_name() {
    let described = Question {
        criteria: &["safe", "unsafe", "unsure"],
        ..question()
    };
    let prompt: Value = serde_json::from_str(&user_prompt(&described, false)).unwrap();
    assert_eq!(prompt["criteria"]["no"], "unsafe");
    let plain: Value = serde_json::from_str(&user_prompt(&question(), false)).unwrap();
    assert!(plain.get("criteria").is_none());
    assert_eq!(
        criteria_pairs(&question()).collect::<Vec<_>>(),
        [("yes", "yes"), ("no", "no"), ("maybe", "maybe")]
    );
}

#[test]
fn laya_judge_asks_one_typed_choice_and_caches_by_checkpoint() {
    use crate::llm::fake;
    let health = json!({"service":"dial-laya","protocol":1,"status":"ok","busy":false,"checkpoint":"/ckpt","device":"cpu"});
    let server = fake::serve(vec![
        fake::json(200, &health),
        fake::json(
            200,
            &json!({"answers":{"choice":{"choice":"medium","confidence":0.8}}}),
        ),
    ]);
    let mut backend = LayaBackend {
        config: crate::config::laya::Config {
            endpoint: server.url.clone(),
            checkpoint: Some("/ckpt".into()),
            ..Default::default()
        },
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = cache::Cache::open(&dir.path().join("j.db"), 100, 10).unwrap();
    let first = judge_thinking(Some(&cache), &mut backend, "laya:/ckpt", "fix the bug", 0).unwrap();
    assert_eq!(first.choice, "medium");
    assert_eq!(first.cost_usd, None);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(fake::request_line(&requests[1]).starts_with("POST /v1/predict"));
    let body = fake::body(&requests[1]);
    let asked = &body["questions"]["choice"];
    assert_eq!(asked["type"], "choice");
    assert_eq!(asked["criteria"]["high"], THINKING_CRITERIA[3]);
    assert_eq!(body["state"]["input"], "fix the bug");
    // A cache hit never reaches the service again.
    let again = judge_thinking(Some(&cache), &mut backend, "laya:/ckpt", "fix the bug", 1).unwrap();
    assert!(again.cached);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn judge_role_assigned_to_laya_is_recognised() {
    use crate::llm::model_config::ModelSelection;
    let mut config = Config::default();
    assert!(!judge_uses_laya(&config));
    config.model_roles.insert(
        "judge".into(),
        ModelSelection::new(
            crate::config::laya::PROVIDER,
            crate::config::laya::JUDGE_MODEL,
        ),
    );
    assert!(judge_uses_laya(&config));
}
