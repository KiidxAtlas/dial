use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn workspace(task: &Task) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, content) in &task.files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, content).expect("write");
    }
    dir
}

fn leaves(check: &Check) -> Vec<&Check> {
    match check {
        Check::Repository { .. } => {
            panic!("operator repository tasks have no generated reference solution")
        }
        Check::All { checks } => checks.iter().flat_map(leaves).collect(),
        other => vec![other],
    }
}

fn has_command(check: &Check) -> bool {
    leaves(check)
        .iter()
        .any(|c| matches!(c, Check::Command { .. }))
}

fn check_paths(check: &Check) -> Vec<&str> {
    leaves(check)
        .into_iter()
        .filter_map(|c| match c {
            Check::FileEquals { path, .. }
            | Check::FileContains { path, .. }
            | Check::FileAbsent { path }
            | Check::JsonFile { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

fn one_per_family(seed: u64) -> Vec<Task> {
    FAMILIES
        .iter()
        .map(|f| (f.build)(&mut StdRng::seed_from_u64(mix(&[seed, 77]))))
        .collect()
}

/// Runs `f` on every item using scoped threads; a panic in `f` fails the test.
fn par_each<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    let next = AtomicUsize::new(0);
    // Libtest already runs tests concurrently, and these workers can each spawn a sandboxed
    // interpreter. Multiplying every CPU by every test can exhaust process/timeout budgets.
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(4)
        .min(items.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                    f(item);
                }
            });
        }
    });
}

#[test]
fn untouched_fails_and_reference_passes_for_every_family() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    let case = |seed: u64, t: &Task| {
        let ws = workspace(t);
        assert!(
            !t.check.verify(ws.path(), ""),
            "{} untouched/empty passes: {}",
            t.family,
            t.prompt
        );
        assert!(
            !t.check.verify(ws.path(), "wrong"),
            "{} untouched/wrong passes",
            t.family
        );
        let reply = reference_solution(t, ws.path());
        assert!(
            t.check.verify(ws.path(), &reply),
            "{} seed {seed}: reference fails\n{}\n{:?}",
            t.family,
            t.prompt,
            t.check
        );
        let info = FAMILIES
            .iter()
            .find(|f| f.name == t.family)
            .expect("family");
        if !info.needs_tools {
            // Guard answers are verifiable without touching the workspace.
            assert!(
                t.check.verify(workspace(t).path(), &reply),
                "{} guard reply must pass on a fresh workspace",
                t.family
            );
        }
    };
    let plain: Vec<u64> = (0..500).collect();
    par_each(&plain, |&seed| {
        for t in one_per_family(seed)
            .iter()
            .filter(|t| !has_command(&t.check))
        {
            case(seed, t);
        }
    });
    // Command checks run python in a sandbox: fewer seeds, one (seed, family) per work item.
    let commands: Vec<(u64, Task)> = (0..12)
        .flat_map(|seed| {
            one_per_family(seed)
                .into_iter()
                .filter(|t| has_command(&t.check))
                .map(move |t| (seed, t))
        })
        .collect();
    par_each(&commands, |(seed, t)| case(*seed, t));
}

#[test]
fn corrupting_any_solved_outcome_fails_the_check() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    let case = |t: &Task| {
        let ws = workspace(t);
        let reply = reference_solution(t, ws.path());
        assert!(
            t.check.verify(ws.path(), &reply),
            "{} reference must pass before corruption",
            t.family
        );
        if let Check::AnswerExact { expected } = &t.check {
            assert!(
                !t.check.verify(ws.path(), &format!("{expected}0")),
                "{} accepts a perturbed answer",
                t.family
            );
        }
        let has_answer = leaves(&t.check)
            .iter()
            .any(|c| matches!(c, Check::AnswerExact { .. }));
        if has_answer {
            assert!(
                !t.check.verify(ws.path(), &format!("{reply} extra")),
                "{} accepts a wrong final answer",
                t.family
            );
        }
        for leaf in leaves(&t.check) {
            match leaf {
                Check::FileEquals { path, .. } => {
                    let full = ws.path().join(path);
                    let good = fs::read_to_string(&full).expect("solved file");
                    fs::write(&full, format!("{good}\n# stray edit\n")).expect("write");
                    assert!(
                        !t.check.verify(ws.path(), &reply),
                        "{} accepts a modified {path}",
                        t.family
                    );
                    fs::remove_file(&full).expect("rm");
                    assert!(
                        !t.check.verify(ws.path(), &reply),
                        "{} accepts a missing {path}",
                        t.family
                    );
                    fs::write(&full, good).expect("restore");
                }
                Check::FileAbsent { path } => {
                    let full = ws.path().join(path);
                    fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
                    fs::write(&full, "x").expect("write");
                    assert!(
                        !t.check.verify(ws.path(), &reply),
                        "{} accepts a leftover {path}",
                        t.family
                    );
                    fs::remove_file(&full).expect("rm");
                }
                Check::JsonFile { path, pointer, .. } => {
                    let full = ws.path().join(path);
                    let good = fs::read_to_string(&full).expect("solved json");
                    let mut doc: Value = serde_json::from_str(&good).expect("json");
                    set_pointer(&mut doc, pointer, json!("__wrong__"));
                    fs::write(&full, doc.to_string()).expect("write");
                    assert!(
                        !t.check.verify(ws.path(), &reply),
                        "{} accepts wrong value at {pointer}",
                        t.family
                    );
                    fs::write(&full, good).expect("restore");
                }
                _ => {}
            }
        }
        assert!(
            t.check.verify(ws.path(), &reply),
            "{} restore should pass again",
            t.family
        );
    };
    let plain: Vec<u64> = (0..120).collect();
    par_each(&plain, |&seed| {
        for t in one_per_family(seed)
            .iter()
            .filter(|t| !has_command(&t.check))
        {
            case(t);
        }
    });
    let commands: Vec<Task> = (0..8)
        .flat_map(one_per_family)
        .filter(|t| has_command(&t.check))
        .collect();
    par_each(&commands, |t| case(t));
}

#[test]
fn plausible_wrong_solutions_fail() {
    for seed in 0..80u64 {
        for t in one_per_family(seed) {
            let ws = workspace(&t);
            match t.family {
                "rename_symbol" => {
                    let (path, _) = t.files.iter().next().expect("file");
                    let Check::FileEquals {
                        content: expected, ..
                    } = &t.check
                    else {
                        panic!("check shape")
                    };
                    let src = &t.files[path];
                    // Naive substring rename of the prefix also mangles the helper.
                    let old_verb = src
                        .split("_values")
                        .next()
                        .and_then(|s| s.rsplit(|c: char| !c.is_alphanumeric()).next())
                        .expect("verb");
                    let new_verb = expected
                        .split("_values")
                        .next()
                        .and_then(|s| s.rsplit(|c: char| !c.is_alphanumeric()).next())
                        .expect("verb");
                    fs::write(ws.path().join(path), src.replace(old_verb, new_verb))
                        .expect("write");
                    assert!(!t.check.verify(ws.path(), ""), "naive rename must fail");
                }
                "replace_value" => {
                    let (path, text) = t.files.iter().next().expect("file");
                    let Check::FileEquals {
                        content: expected, ..
                    } = &t.check
                    else {
                        panic!("check shape")
                    };
                    // A blind value replacement must fail if another setting shares that value.
                    let (before, after) = text
                        .lines()
                        .zip(expected.lines())
                        .find(|(a, b)| a != b)
                        .expect("changed setting");
                    let value = |line: &str| {
                        line.split_once(['=', ':'])
                            .expect("setting")
                            .1
                            .trim()
                            .to_string()
                    };
                    let wrong = text.replace(&value(before), &value(after));
                    if wrong != *expected {
                        fs::write(ws.path().join(path), wrong).expect("write");
                        assert!(
                            !t.check.verify(ws.path(), ""),
                            "changing a decoy setting must fail"
                        );
                    }
                    fs::write(ws.path().join(path), text).expect("write");
                    assert!(
                        !t.check.verify(ws.path(), ""),
                        "leaving the old value must fail"
                    );
                }
                "move_file" => {
                    // Copying instead of moving leaves the source behind.
                    let Check::All { checks } = &t.check else {
                        panic!("check shape")
                    };
                    for c in checks {
                        if let Check::FileEquals { path, content } = c {
                            write_file(ws.path(), path, content);
                        }
                    }
                    assert!(
                        !t.check.verify(ws.path(), ""),
                        "copy must not count as a move"
                    );
                }
                "count_matches" | "list_by_ext" | "read_line" | "csv_lookup" | "run_compute" => {
                    let Check::AnswerExact { expected } = &t.check else {
                        panic!("check shape")
                    };
                    let off: String = expected
                        .parse::<i64>()
                        .map(|n| (n + 1).to_string())
                        .unwrap_or_else(|_| format!("{expected},extra"));
                    assert!(!t.check.verify(ws.path(), &off));
                    assert!(!t.check.verify(ws.path(), ""));
                }
                _ => {}
            }
        }
    }
}

#[test]
fn fix_bug_rejects_hard_coded_output_and_typo_hunt_rejects_wrong_fix() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    par_each(&(0..10u64).collect::<Vec<_>>(), |&seed| {
        let mut rng = StdRng::seed_from_u64(seed);
        let t = fix_bug_py(&mut rng);
        let ws = workspace(&t);
        let Check::Command { expect_stdout, .. } = &t.check else {
            panic!("check shape")
        };
        let first = expect_stdout
            .as_deref()
            .and_then(|s| s.lines().next())
            .expect("expected");
        let (path, _) = t.files.iter().next().expect("file");
        fs::write(ws.path().join(path), format!("print({first})\n")).expect("write");
        assert!(
            !t.check.verify(ws.path(), ""),
            "hard-coded print must fail the hidden probes: {}",
            t.prompt
        );
        let source = &t.files[path];
        let function = source.lines().next().expect("function signature");
        fs::write(ws.path().join(path), format!("{function}\n    return {first}\n\nif __name__ == \"__main__\":\n    print({first})\n")).expect("write");
        assert!(
            !t.check.verify(ws.path(), ""),
            "constant-return function must fail the hidden probes"
        );
        fs::write(ws.path().join(path), source).expect("restore");
        reference_solution(&t, ws.path());
        assert!(t.check.verify(ws.path(), ""), "repaired function must pass");

        let t = typo_hunt(&mut StdRng::seed_from_u64(seed));
        let ws = workspace(&t);
        // Even after correcting the typo, an unrelated change must fail.
        reference_solution(&t, ws.path());
        assert!(
            t.check.verify(ws.path(), ""),
            "correct typo repair must pass"
        );
        let victim = leaves(&t.check)
            .into_iter()
            .find_map(|c| match c {
                Check::FileEquals { path, content }
                    if content.starts_with("def ") && t.files.get(path) == Some(content) =>
                {
                    Some(path)
                }
                _ => None,
            })
            .expect("unchanged module");
        let good = fs::read_to_string(ws.path().join(victim)).expect("read");
        fs::write(ws.path().join(victim), good.replace("def ", "def  ")).expect("write");
        assert!(!t.check.verify(ws.path(), ""), "unrelated edit must fail");
    });
}

#[test]
fn command_checks_really_run_python() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    let t = fix_bug_py(&mut StdRng::seed_from_u64(3));
    let ws = workspace(&t);
    assert!(!t.check.verify(ws.path(), ""));
    reference_solution(&t, ws.path());
    assert!(t.check.verify(ws.path(), ""));
    let Check::Command { command, .. } = &t.check else {
        panic!("check shape")
    };
    let wrong = Check::Command {
        command: command.clone(),
        expect_stdout: Some("nonsense".into()),
        expect_exit: 0,
    };
    assert!(
        !wrong.verify(ws.path(), ""),
        "a Command check must compare real stdout"
    );

    let t = script_and_run(&mut StdRng::seed_from_u64(5));
    let ws = workspace(&t);
    let reply = reference_solution(&t, ws.path());
    assert!(t.check.verify(ws.path(), &reply));
    // Verification restores the data file it temporarily replaces, so it is repeatable.
    assert!(t.check.verify(ws.path(), &reply));
    for (path, content) in &t.files {
        assert_eq!(
            &fs::read_to_string(ws.path().join(path)).expect("data"),
            content
        );
        fs::remove_file(ws.path().join(path)).expect("remove data");
        assert!(
            !t.check.verify(ws.path(), &reply),
            "missing input must fail"
        );
        write_file(ws.path(), path, content);
    }
    assert!(
        t.check.verify(ws.path(), &reply),
        "restored input must pass"
    );
    let command = leaves(&t.check)
        .into_iter()
        .find_map(|c| match c {
            Check::Command { command, .. } => Some(command),
            _ => None,
        })
        .expect("script command");
    let words: Vec<&str> = command.split_whitespace().collect();
    let script =
        words[words.iter().position(|w| *w == "-B").expect("python") + 1].trim_end_matches(';');
    fs::write(ws.path().join(script), format!("print({reply})\n")).expect("write constant script");
    assert!(
        !t.check.verify(ws.path(), &reply),
        "hard-coded script output must fail"
    );
    for (path, content) in &t.files {
        assert_eq!(
            &fs::read_to_string(ws.path().join(path)).expect("restored data"),
            content
        );
    }
}

#[test]
fn generation_is_deterministic_and_prompts_are_distinct() {
    for role in [Role::SelfPlay, Role::Primary, Role::Guard, Role::Anchor] {
        for seed in [0u64, 1, 42, 9999] {
            let a = generate(role, 60, seed);
            let b = generate(role, 60, seed);
            assert_eq!(a.len(), 60, "{role:?} produced fewer tasks than requested");
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(
                    (x.family, &x.prompt, &x.files, &x.check),
                    (y.family, &y.prompt, &y.files, &y.check)
                );
            }
            let prompts: BTreeSet<&String> = a.iter().map(|t| &t.prompt).collect();
            assert_eq!(
                prompts.len(),
                a.len(),
                "{role:?} seed {seed}: duplicate prompts"
            );
        }
        assert_ne!(
            generate(role, 10, 1)
                .iter()
                .map(|t| t.prompt.clone())
                .collect::<Vec<_>>(),
            generate(role, 10, 2)
                .iter()
                .map(|t| t.prompt.clone())
                .collect::<Vec<_>>(),
            "{role:?}: different seeds should give different tasks"
        );
    }
}

#[test]
fn failure_variants_are_isolated_and_deterministic() {
    let priorities = [
        "config_precedence",
        "negative_search",
        "semantic_validation",
    ]
    .map(str::to_owned);
    for seed in [0, 1, 42, u64::MAX] {
        let train = generate_targeted(Role::SelfPlay, 24, seed, &priorities);
        let held_out = generate_failure_holdout(12, seed);
        let repeated = generate_failure_holdout(12, seed);
        assert_eq!(held_out.len(), 12);
        assert!(
            held_out
                .iter()
                .zip(repeated)
                .all(|(a, b)| a.prompt == b.prompt && a.files == b.files && a.check == b.check)
        );
        assert!(
            held_out
                .iter()
                .all(|case| !train.iter().any(|task| task.prompt == case.prompt))
        );
        let forbidden = held_out
            .iter()
            .map(|task| task.family.to_owned())
            .collect::<Vec<_>>();
        let rejected = generate_targeted(Role::SelfPlay, 24, seed, &forbidden);
        assert!(
            rejected
                .iter()
                .all(|task| !forbidden.iter().any(|family| family == task.family))
        );
        for family in &priorities {
            assert!(
                train
                    .iter()
                    .filter(|task| task.family == family.as_str())
                    .count()
                    >= 4
            );
        }
    }
}

#[test]
fn syntax_success_and_empty_search_are_not_semantic_success() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    par_each(
        &[
            config_precedence as Build,
            config_layers,
            negative_search,
            search_recovery,
            semantic_validation,
            semantic_boundaries,
        ],
        |&build| {
            let task = build(&mut StdRng::seed_from_u64(21));
            let workspace = workspace(&task);
            let path = task
                .files
                .keys()
                .find(|path| path.ends_with(".py"))
                .unwrap();
            let syntax = Check::Command {
                command: format!(
                    "python3 -B -c 'import ast; ast.parse(open({path:?}).read()); print(\"syntax-ok\")'"
                ),
                expect_stdout: Some("syntax-ok".into()),
                expect_exit: 0,
            };
            assert!(
                syntax.verify(workspace.path(), ""),
                "{} must begin syntactically valid",
                task.family
            );
            assert!(
                !task.check.verify(workspace.path(), ""),
                "{} must fail behavior",
                task.family
            );
            reference_solution(&task, workspace.path());
            assert!(
                task.check.verify(workspace.path(), ""),
                "{} reference",
                task.family
            );
            if matches!(task.family, "negative_search" | "search_recovery") {
                fs::write(
                    workspace.path().join(path),
                    "def search(directory, needle):\n    return []\n",
                )
                .unwrap();
                assert!(
                    !task.check.verify(workspace.path(), ""),
                    "empty results must not disguise operational errors"
                );
            } else if matches!(task.family, "semantic_validation" | "semantic_boundaries") {
                fs::write(workspace.path().join(path), "def select(values, lower, upper):\n    return sorted(set(v for v in values if lower <= v <= upper))\n").unwrap();
                assert!(
                    !task.check.verify(workspace.path(), ""),
                    "syntax-valid sorting/deduplication is not the contract"
                );
            }
        },
    );
}

#[test]
fn targeted_generation_prioritizes_only_distinct_train_tool_families() {
    let priorities = vec!["json_set".to_string(), "fix_bug_py".to_string()];
    let targeted = generate_targeted(Role::SelfPlay, 44, 23, &priorities);
    let normal = generate(Role::SelfPlay, 44, 23);
    let counts = |tasks: &[Task]| {
        tasks
            .iter()
            .fold(BTreeMap::<&str, usize>::new(), |mut counts, task| {
                *counts.entry(task.family).or_default() += 1;
                counts
            })
    };
    let target_counts = counts(&targeted);
    let normal_counts = counts(&normal);
    for family in ["json_set", "fix_bug_py"] {
        assert!(
            target_counts.get(family).copied().unwrap_or_default()
                > normal_counts.get(family).copied().unwrap_or_default(),
            "{family} was not prioritized"
        );
    }

    let split = |name: &str| {
        FAMILIES
            .iter()
            .find(|family| family.name == name)
            .expect("family")
    };
    assert!(
        targeted
            .iter()
            .all(|task| split(task.family).needs_tools && split(task.family).split == Split::Train)
    );
    assert_eq!(
        targeted
            .iter()
            .map(|task| &task.prompt)
            .collect::<BTreeSet<_>>()
            .len(),
        targeted.len()
    );
    let repeated = generate_targeted(Role::SelfPlay, 44, 23, &priorities);
    assert!(
        targeted
            .iter()
            .zip(repeated)
            .all(|(left, right)| left.family == right.family && left.prompt == right.prompt)
    );

    let ignored = vec![
        "sum_two_files".to_string(),
        "format_only".to_string(),
        "not_a_family".to_string(),
        "json_set".to_string(),
        "json_set".to_string(),
    ];
    let from_mixed = generate_targeted(Role::SelfPlay, 44, 23, &ignored);
    let from_valid = generate_targeted(Role::SelfPlay, 44, 23, &["json_set".to_string()]);
    assert_eq!(
        from_mixed
            .iter()
            .map(|task| task.family)
            .collect::<Vec<_>>(),
        from_valid
            .iter()
            .map(|task| task.family)
            .collect::<Vec<_>>()
    );
    assert!(
        from_mixed
            .iter()
            .all(|task| task.family != "sum_two_files" && task.family != "format_only")
    );
    assert_eq!(
        generate_targeted(Role::Primary, 12, 23, &priorities)
            .iter()
            .map(|task| task.prompt.clone())
            .collect::<Vec<_>>(),
        generate(Role::Primary, 12, 23)
            .iter()
            .map(|task| task.prompt.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn roles_draw_from_the_right_splits() {
    let split = |name: &str| FAMILIES.iter().find(|f| f.name == name).expect("family");
    for seed in 0..5u64 {
        let train_count = FAMILIES
            .iter()
            .filter(|f| f.needs_tools && f.split == Split::Train)
            .count();
        let selfplay = generate(Role::SelfPlay, train_count * 4, seed);
        assert!(
            selfplay
                .iter()
                .all(|t| split(t.family).split == Split::Train && split(t.family).needs_tools)
        );
        let mut per_family: BTreeMap<&str, usize> = BTreeMap::new();
        selfplay
            .iter()
            .for_each(|t| *per_family.entry(t.family).or_default() += 1);
        assert_eq!(per_family.len(), train_count, "every train family appears");
        assert!(
            per_family.values().all(|c| *c == 4),
            "round-robin is even: {per_family:?}"
        );

        let primary = generate(Role::Primary, 100, seed);
        let gate = primary
            .iter()
            .filter(|t| split(t.family).split == Split::Gate)
            .count();
        assert_eq!(gate, 40);
        assert!(
            primary.iter().all(|t| split(t.family).needs_tools),
            "primary needs tools"
        );
        let gate_families: BTreeSet<&str> = primary
            .iter()
            .filter(|t| split(t.family).split == Split::Gate)
            .map(|t| t.family)
            .collect();
        assert_eq!(
            gate_families.len(),
            FAMILIES
                .iter()
                .filter(|f| f.needs_tools && f.split == Split::Gate && !is_hard(f.name))
                .count(),
            "all tier-0 gate families covered"
        );

        let guard = generate(Role::Guard, 80, seed);
        assert!(
            guard
                .iter()
                .all(|t| !split(t.family).needs_tools && t.files.len() <= 1)
        );
        let covered: BTreeSet<&str> = guard.iter().map(|t| t.family).collect();
        assert_eq!(
            covered.len(),
            FAMILIES.iter().filter(|f| !f.needs_tools).count()
        );

        let anchor = generate(Role::Anchor, 48, seed);
        let covered: BTreeSet<&str> = anchor.iter().map(|t| t.family).collect();
        assert_eq!(covered.len(), FAMILIES.len(), "anchor spans everything");
    }
    // Anchor composition is independent of the seed: only instances change.
    let fams = |s| {
        generate(Role::Anchor, 30, s)
            .iter()
            .map(|t| t.family)
            .collect::<Vec<_>>()
    };
    assert_eq!(fams(1), fams(2));
    // Instances are prefix-stable: asking for more does not change earlier ones.
    let short = generate(Role::Anchor, 10, 7);
    let long = generate(Role::Anchor, 30, 7);
    assert!(short.iter().zip(&long).all(|(a, b)| a.prompt == b.prompt));
}

#[test]
fn family_registry_meets_the_minimums() {
    let info = families();
    assert!(
        info.iter()
            .filter(|f| f.needs_tools && f.split == Split::Train)
            .count()
            >= 10
    );
    assert!(
        info.iter()
            .filter(|f| f.needs_tools && f.split == Split::Gate)
            .count()
            >= 5
    );
    assert!(info.iter().any(|f| !f.needs_tools));
    let names: BTreeSet<&str> = info.iter().map(|f| f.name).collect();
    assert_eq!(names.len(), info.len(), "family names are unique");
}

#[test]
fn generated_paths_are_safe() {
    for role in [Role::SelfPlay, Role::Primary, Role::Guard, Role::Anchor] {
        for seed in 0..40u64 {
            for t in generate(role, 48, seed) {
                for path in t
                    .files
                    .keys()
                    .map(|p| p.as_str())
                    .chain(check_paths(&t.check))
                {
                    assert!(safe_rel(path), "{} unsafe path {path:?}", t.family);
                }
            }
        }
    }
}

#[test]
fn advance_tier_promotes_at_ninety_percent_and_caps_at_max() {
    assert_eq!(advance_tier(0, 899, 1000), 0, "89.9% must stay on tier 0");
    assert_eq!(advance_tier(0, 9, 10), 1, "exactly 90.0% advances");
    assert_eq!(advance_tier(0, 10, 10), 1);
    assert_eq!(advance_tier(1, 100, 100), 2);
    assert_eq!(advance_tier(2, 100, 100), 2, "tier is capped");
    assert_eq!(advance_tier(0, 0, 0), 0, "an empty baseline never advances");
}

#[test]
fn hard_families_are_gate_only_tools_and_primary_tiers_select_them() {
    for name in HARD_FAMILIES {
        let family = FAMILIES
            .iter()
            .find(|f| f.name == *name)
            .unwrap_or_else(|| panic!("{name} missing from FAMILIES"));
        assert!(family.needs_tools && family.split == Split::Gate, "{name}");
    }
    for seed in 0..20u64 {
        assert!(
            generate_primary_tier(0, 12, seed)
                .iter()
                .all(|t| !is_hard(t.family)),
            "tier 0 must not draw hard families"
        );
        let top = generate_primary_tier(2, 12, seed);
        assert_eq!(top.len(), 12);
        assert!(top.iter().all(|t| is_hard(t.family)), "tier 2 is hard-only");
    }
}
