use super::*;
use serde_json::json;

const CASES: [&str; 8] = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"];

/// Edits `prompt.txt` to the next scripted value; records what it saw on each proposal.
struct Scripted {
    candidates: Vec<i64>,
    seen: Vec<String>,
}

impl Proposer for Scripted {
    fn propose(&mut self, root: &Path, state: &State, _variant: &str) -> Result<Option<String>> {
        let path = root.join(&state.change_paths[0]);
        self.seen.push(fs::read_to_string(&path)?);
        if self.candidates.is_empty() {
            return Ok(None);
        }
        let next = self.candidates.remove(0);
        fs::write(&path, format!("{next}\n"))?;
        Ok(Some(format!("set score to {next}")))
    }
}

/// Fixed score function: every case's grade is the number in `prompt.txt`.
struct FixedScore {
    calls: usize,
}

impl Evaluator for FixedScore {
    fn run(&mut self, root: &Path, check: &Check) -> Result<()> {
        self.calls += 1;
        let value: f64 = fs::read_to_string(root.join("prompt.txt"))?
            .trim()
            .parse()?;
        let dir = root.join(&check.out_dir);
        fs::create_dir_all(&dir)?;
        let mut out = String::new();
        for id in CASES {
            out.push_str(&json!({"prompt_id": id, "rep": 0, "grade": value}).to_string());
            out.push('\n');
        }
        fs::write(dir.join("results.jsonl"), out)?;
        Ok(())
    }
}

#[test]
fn climb_keeps_improvements_reverts_regressions_and_respects_round_budget() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    fs::write(root.join("cases.jsonl"), "cases\n")?;
    fs::write(root.join("run.sh"), "harness\n")?;
    fs::write(root.join("prompt.txt"), "2\n")?;
    let flow = "score";

    init(
        root,
        flow,
        InitOptions {
            cases: vec!["cases.jsonl".into()],
            harness: vec!["run.sh".into()],
            change: vec!["prompt.txt".into()],
            off_limits: vec![],
        },
    )?;
    let mut state = require(root, flow)?;
    plan(
        &mut state,
        PlanOptions {
            goal: Some(GoalOptions {
                target: "score".into(),
                direction: Some(Direction::Higher),
                ..Default::default()
            }),
            reps: Some(1),
            plateau: Some(5),
            rounds: Some(3),
            command: Some("run.sh {variant} {flow_dir}".into()),
        },
    )?;
    let tags: BTreeMap<String, String> = CASES
        .iter()
        .enumerate()
        .map(|(i, id)| (id.to_string(), if i < 4 { "x" } else { "y" }.to_string()))
        .collect();
    split(&mut state, &tags, 0.5, Some(7))?;
    for stage in Stage::ALL {
        approve(root, &mut state, stage)?;
    }
    save(root, &state)?;

    // Scores in order: 5 improves, 3 regresses (reverted), 9 improves; budget is 3 rounds.
    let mut proposer = Scripted {
        candidates: vec![5, 3, 9],
        seen: Vec::new(),
    };
    let mut evaluator = FixedScore { calls: 0 };
    let halt = climb(root, flow, &mut proposer, &mut evaluator)?;

    assert_eq!(halt, Halt::Done);
    assert_eq!(
        proposer.candidates.len(),
        0,
        "all scripted candidates were used"
    );
    assert_eq!(
        proposer.seen,
        vec!["2\n", "5\n", "5\n"],
        "revert restored 5 before the third candidate"
    );
    assert_eq!(evaluator.calls, 4, "one baseline plus three candidates");
    assert_eq!(fs::read_to_string(root.join("prompt.txt"))?, "9\n");

    let state = require(root, flow)?;
    let decisions: Vec<Decision> = state.rounds.iter().map(|round| round.decision).collect();
    assert_eq!(
        decisions,
        vec![
            Decision::Baseline,
            Decision::Keep,
            Decision::Revert,
            Decision::Keep
        ]
    );
    assert_eq!(
        state.best.as_ref().map(|best| best.variant.as_str()),
        Some("v3")
    );
    assert_eq!(state.flat_rounds, 0);
    assert!(finished(&state), "round budget reached");
    Ok(())
}
