use super::*;
use crate::config::settings_store::Change;
use crate::weights::{Operation, run, status};
use std::sync::atomic::AtomicBool;

fn user(text: &str) -> Value {
    json!({"messages": [{"role": "user", "content": text}]})
}

fn cases(prefix: &str, count: usize) -> Value {
    json!(
        (0..count)
            .map(|n| user(&format!("{prefix}{n}")))
            .collect::<Vec<_>>()
    )
}

/// A base checkpoint and a config whose fake worker answers every weights operation: the base
/// always hits the token limit, an edited copy never does.
fn fixture(root: &Path) -> Config {
    let model = root.join("model");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(model.join("config.json"), r#"{"model_type":"llama"}"#).unwrap();
    std::fs::write(model.join("model.safetensors"), "x").unwrap();
    let config = Config {
        state_dir: root.join("state"),
        user_dir: root.join("user"),
        python: PathBuf::from("/usr/bin/python3"),
        worker: root.join("worker.py"),
        model: model.to_string_lossy().into_owned(),
        worker_timeout_secs: 60,
        ..Config::default()
    };
    std::fs::write(
        &config.worker,
        r#"
import json,sys
from pathlib import Path
r=json.load(sys.stdin)
op=r['op']
if op=='weights_generate':
    edited=(Path(r['model'])/'weights-edit.json').exists()
    rows=[{'case':i,'tokens':5,'hit_limit':not edited,'passed':None,'repeat_ratio':0.0} for i in range(len(r['cases']))]
    print(json.dumps({'op':'generate','model':r['model'],'model_id':'edited' if edited else 'base','max_tokens':512,
                      'cases':rows,'summary':{'cases':len(rows),'hit_limit':0 if edited else len(rows)}}))
elif op=='weights_direction':
    Path(r['output']).write_bytes(b'x')
    print(json.dumps({'op':'direction','model':r['model'],'layers':[]}))
elif op=='weights_edit':
    out=Path(r['output']); out.mkdir()
    (out/'config.json').write_text('{"model_type":"llama"}')
    (out/'model.safetensors').write_text('x')
    (out/'weights-edit.json').write_text(json.dumps({'base':r['model'],'base_id':'base'}))
    print(json.dumps({'op':'edit','model':r['model'],'output':str(out),'layers':[]}))
elif op=='weights_combine':
    out=Path(r['output']); out.mkdir()
    (out/'config.json').write_text('{"model_type":"llama"}')
    (out/'model.safetensors').write_text('x')
    (out/'weights-edit.json').write_text(json.dumps({'method':'combine','base_id':'base'}))
    print(json.dumps({'op':'combine','model':r['model'],'method':'combine','output':str(out),
                      'sources':r['edits'],'changes':[],'edited_params':3}))
"#,
    )
    .unwrap();
    config
}

fn set_model(changes: &[Change]) -> Option<String> {
    changes.iter().find_map(|change| match change {
        Change::Set { path, value } if path == "model" => value.as_str().map(str::to_owned),
        _ => None,
    })
}

/// The error of a refused model switch (`ModelSwitch` holds settings changes, which have no Debug).
fn refused(result: Result<ModelSwitch>) -> String {
    match result {
        Ok(switch) => panic!("switch was allowed: {}", switch.message),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn prompt_hashes_identify_prompts_but_not_direction_answers() {
    let a = prompts(Operation::Generate, &json!({"cases": [user("q")]}));
    assert_eq!(
        a,
        prompts(Operation::Generate, &json!({"cases": [user("q")]}))
    );
    assert_ne!(
        a,
        prompts(Operation::Generate, &json!({"cases": [user("r")]}))
    );
    assert_eq!(a[0].len(), 64, "a SHA-256 hex digest, never prompt text");
    let answered = |answer: &str| json!({"messages": [{"role": "user", "content": "q"}, {"role": "assistant", "content": answer}]});
    let direction = prompts(
        Operation::Direction,
        &json!({"positive": [answered("good")], "negative": [answered("loop loop")]}),
    );
    // Both sides share the prompt `q`, which is exactly the generation case `q`.
    assert_eq!(direction, vec![a[0].clone(), a[0].clone()]);
    assert!(prompts(Operation::Edit, &json!({})).is_empty());
}

#[test]
fn comparison_admits_only_significant_wins_over_enough_cases() {
    let result = |hits: &[bool]| {
        json!({"cases": hits.iter().map(|hit| json!({"hit_limit": hit, "passed": null})).collect::<Vec<_>>(),
               "summary": {}})
    };
    // (base hit-limit per case, edited hit-limit per case, passed, reason fragment)
    let table: &[(&[bool], &[bool], bool, &str)] = &[
        (&[true; 8], &[false; 8], true, ""),
        (&[true; 7], &[false; 7], false, "at least 8"),
        (&[false; 8], &[true; 8], false, "did not win"),
        (
            &[true, true, false, false, false, false, false, false],
            &[false; 8],
            false,
            "not significant",
        ),
    ];
    for (base, edited, passed, reason) in table {
        let value = compare(&result(base), &result(edited)).unwrap();
        assert_eq!(value["passed"], *passed, "{value}");
        assert!(
            value["reason"]
                .as_str()
                .unwrap_or_default()
                .contains(reason),
            "{value}"
        );
    }
    let value = compare(&result(&[true; 8]), &result(&[false; 8])).unwrap();
    assert_eq!(
        (value["wins"].as_u64(), value["losses"].as_u64()),
        (Some(8), Some(0))
    );
    // A failed response check makes a case bad even when it stopped in time.
    assert!(!good(&json!({"hit_limit": false, "passed": false})));
    assert!(compare(&result(&[true]), &result(&[true, true])).is_err());
}

#[test]
fn probe_loss_rise_blocks_an_otherwise_significant_edit() {
    let result = |hit: bool, loss: Option<f64>| {
        let mut value = json!({"cases": vec![json!({"hit_limit": hit, "passed": null}); 8],
                               "summary": {}});
        if let Some(loss) = loss {
            value["probe_loss"] = json!(loss);
        }
        value
    };
    // (base probe loss, edited probe loss, passed)
    let table = [
        (Some(2.0), Some(2.05), true),
        (Some(2.0), Some(2.2), false),
        (Some(2.0), Some(1.5), true),
        (None, Some(9.0), true),
    ];
    for (before, after, passed) in table {
        let value = admit_for_tests(&result(true, before), &result(false, after));
        assert_eq!(value["passed"], passed, "{value}");
        if !passed {
            assert!(value["reason"].as_str().unwrap().contains("probe text"));
        }
        assert_eq!(value["quality"].is_null(), before.is_none(), "{value}");
    }
}

#[test]
fn records_written_before_history_still_load() {
    let ledger: Ledger = serde_json::from_value(json!({"name": "old", "budget": 2, "used": 1,
        "created_at": 1, "runs": [{"op": "generate", "at": 1, "cost": 1, "prompts": []}]}))
    .unwrap();
    assert!(ledger.runs[0].result.is_none());
    let record: EditRecord = serde_json::from_value(json!({"name": "e", "experiment": "x",
        "direction": "d", "base_model": "m", "base": "/b", "output": "/o", "created_at": 1,
        "evaluation": {"passed": true, "wins": 8, "fingerprint": "f"}}))
    .unwrap();
    assert!(record.evaluation.unwrap()["rows"].is_null());
}

#[cfg(unix)]
#[test]
fn combine_sums_edits_of_one_experiment_into_a_new_unevaluated_copy() {
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path());
    let cancel = AtomicBool::new(false);
    create_experiment(&config, "mix", 4).unwrap();
    for name in ["a", "b"] {
        let input = json!({"experiment": "mix", "name": name, "method": "scale",
                           "changes": [{"tensor": "t", "scale": 0.5}]});
        run(&config, None, Operation::Edit, &input, None, &cancel).unwrap();
    }
    let combine_with = |input: Value| combine(&config, None, &input, &cancel);
    assert!(combine_with(json!({"name": "ab", "edits": ["a"]})).is_err());
    assert!(combine_with(json!({"name": "ab", "edits": ["a", "a"]})).is_err());
    assert!(combine_with(json!({"name": "ab", "edits": ["a", "missing"]})).is_err());
    let value = combine_with(json!({"name": "ab", "edits": ["a", "b"]})).unwrap();
    assert_eq!(value["op"], "combine");
    assert!(value["run_id"].is_string());
    let state = status(&config);
    let made = &state["edits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edit| edit["name"] == "ab")
        .unwrap()
        .clone();
    assert_eq!(made["experiment"], "mix");
    assert_eq!(made["details"]["method"], "combine");
    assert_eq!(made["details"]["sources"], json!(["a", "b"]));
    assert!(
        made["passed"].is_null(),
        "a combination needs its own evaluation"
    );
    let again = format!(
        "{:#}",
        combine_with(json!({"name": "ab", "edits": ["a", "b"]})).unwrap_err()
    );
    assert!(again.contains("already exists"), "{again}");
}

#[cfg(unix)]
#[test]
fn experiment_budget_edit_evaluation_promotion_and_revert() {
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path());
    let cancel = AtomicBool::new(false);
    let answered = json!([{"messages": [{"role": "user", "content": "q0"}, {"role": "assistant", "content": "a"}]}]);

    assert!(create_experiment(&config, "loops", 0).is_err());
    let created = create_experiment(&config, "loops", 2).unwrap();
    assert_eq!(
        (created["budget"].as_u64(), created["used"].as_u64()),
        (Some(2), Some(0))
    );
    assert!(
        create_experiment(&config, "loops", 2).is_err(),
        "names are unique"
    );

    let generate = |input: Value| run(&config, None, Operation::Generate, &input, None, &cancel);
    let error = generate(json!({"cases": cases("q", 1)})).unwrap_err();
    assert!(format!("{error:#}").contains("belong to an experiment"));
    let value = generate(json!({"experiment": "loops", "cases": cases("q", 1)})).unwrap();
    assert_eq!(value["experiment"]["used"], 1);

    let direction =
        json!({"experiment": "loops", "name": "loop", "positive": answered, "negative": answered});
    run(
        &config,
        None,
        Operation::Direction,
        &direction,
        None,
        &cancel,
    )
    .unwrap();
    let error = generate(json!({"experiment": "loops", "cases": cases("q", 1)})).unwrap_err();
    assert!(
        format!("{error:#}").contains("used its budget"),
        "{error:#}"
    );
    assert_eq!(
        experiment(&config, "loops").unwrap()["used"],
        2,
        "a refused run is not charged"
    );

    // Edits cost nothing, but only use directions from their own experiment.
    create_experiment(&config, "other", 1).unwrap();
    let edit = |experiment: &str, name: &str| {
        let input =
            json!({"experiment": experiment, "name": name, "direction": "loop", "layers": [0]});
        run(&config, None, Operation::Edit, &input, None, &cancel)
    };
    assert!(format!("{:#}", edit("other", "e0").unwrap_err()).contains("own experiment"));
    edit("loops", "e1").unwrap();
    assert!(edit("loops", "e1").is_err(), "edit names are unique");
    assert_eq!(status(&config)["edits"][0]["name"], "e1");

    assert!(refused(promotion(&config, "e1")).contains("not been evaluated"));

    // Evaluation refuses cases the experiment saw (q0 was generated and used for the direction).
    let evaluate_with = |cases: Value| {
        evaluate(
            &config,
            None,
            &json!({"edit": "e1", "cases": cases}),
            &cancel,
        )
    };
    assert!(format!("{:#}", evaluate_with(cases("held", 4)).unwrap_err()).contains("at least 8"));
    assert!(format!("{:#}", evaluate_with(cases("q", 8)).unwrap_err()).contains("held-out"));
    let value = evaluate_with(cases("held", 8)).unwrap();
    assert_eq!(value["passed"], true, "{value}");
    assert_eq!(value["wins"], 8);
    assert_eq!(status(&config)["evaluate"]["edit"], "e1");
    // The evaluation is kept per edit with its rows, and every run is in the history.
    let state = status(&config);
    let kept = &state["edits"][0]["evaluation"];
    assert_eq!(kept["rows"].as_array().map(Vec::len), Some(8), "{kept}");
    assert!(
        kept.get("fingerprint").is_none(),
        "fingerprint stays internal"
    );
    assert_eq!(state["runs"][0]["op"], "evaluate");
    assert_eq!(state["runs"][0]["id"], value["run_id"]);
    let charged = &state["experiments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["experiment"] == "loops")
        .unwrap()["runs"];
    let linked = charged[0]["result"].as_str().unwrap();
    assert_eq!(
        crate::weights::show_run(&config, linked).unwrap()["op"],
        "generate"
    );
    // Held-out cases stay out of the experiment afterwards.
    create_experiment(&config, "more", 1).unwrap();
    let ledger_error = {
        let mut ledger = ledger(&config, "loops").unwrap();
        ledger.budget = 3;
        save(&experiment_path(&config, "loops"), &ledger).unwrap();
        generate(json!({"experiment": "loops", "cases": cases("held", 1)})).unwrap_err()
    };
    assert!(
        format!("{ledger_error:#}").contains("held out"),
        "{ledger_error:#}"
    );

    let output = edits_dir(&config).join("e1");
    let Ok(switch) = promotion(&config, "e1") else {
        panic!("a passing, unchanged edit can be promoted")
    };
    assert_eq!(set_model(&switch.changes).as_deref(), output.to_str());
    std::fs::write(output.join("extra.txt"), "changed").unwrap();
    assert!(refused(promotion(&config, "e1")).contains("changed after"));
    std::fs::remove_file(output.join("extra.txt")).unwrap();

    assert!(refused(reversion(&config)).contains("not a Dial weight edit"));
    let promoted = Config {
        model: output.to_string_lossy().into_owned(),
        ..config.clone()
    };
    let Ok(switch) = reversion(&promoted) else {
        panic!("a promoted edit can be reverted")
    };
    assert_eq!(set_model(&switch.changes), Some(config.model.clone()));
}
