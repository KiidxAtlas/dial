use super::*;

fn task(family: &'static str, prompt: &str) -> Task {
    let mut generated = curriculum::generate(Role::SelfPlay, 1, 1);
    let mut task = generated.remove(0);
    task.family = family;
    task.prompt = prompt.into();
    task
}

#[test]
fn distills_only_from_weak_train_families() {
    let rates = vec![
        ("selfplay:read_line".to_owned(), 1, 10),
        ("selfplay:create_file".to_owned(), 9, 10),
        ("selfplay:sum_two_files".to_owned(), 0, 5),
    ];
    let weak = weak_families(&rates);
    assert_eq!(weak, vec!["read_line".to_owned()]);
}

#[test]
fn gate_and_anchor_families_never_become_teacher_tasks() {
    let gate = curriculum::families()
        .into_iter()
        .find(|f| f.split == Split::Gate && f.needs_tools)
        .expect("a gate family with tools exists")
        .name;
    let weak = vec!["read_line".to_owned(), gate.to_owned()];
    let generated = vec![
        task(gate, "gate prompt"),
        task("read_line", "train prompt"),
        task("create_file", "not weak"),
    ];
    let selected = select_tasks(generated, &weak, &HashSet::new(), 8);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].family, "read_line");
}

#[test]
fn held_out_prompts_are_excluded_and_cap_is_respected() {
    let held: HashSet<String> = ["sealed prompt".to_owned()].into();
    let generated = vec![
        task("read_line", " sealed prompt "),
        task("read_line", "a"),
        task("read_line", "b"),
        task("read_line", "c"),
    ];
    let weak = vec!["read_line".to_owned()];
    let selected = select_tasks(generated, &weak, &held, 2);
    let prompts: Vec<&str> = selected.iter().map(|t| t.prompt.as_str()).collect();
    assert_eq!(prompts, vec!["a", "b"]);
}

#[test]
fn origin_tag_has_no_separator_and_distillation_is_off_by_default() {
    assert_eq!(origin_tag("org/Model:7b"), "teacher-org_Model_7b");
    assert!(!origin_tag("a:b").contains(':'));
    assert!(!Config::default().improve.distill);
    assert!(!enabled(&Config::default()));
}
