//! Procedurally generated, deterministic-per-seed agentic tasks with verifiers. This is the
//! external ground truth that lets dial improve with no human-provided data: every task carries a
//! `Check` (`learn/verify.rs`) that judges the final workspace and the agent's final answer, and a
//! reference solution exists for every task so the verifiers themselves can be self-tested.
//! Executable tasks probe both visible and unseen inputs with differing results, so returning a
//! memorized visible answer cannot substitute for fixing or implementing the requested computation.
//!
//! Family kinds:
//! * Train tool families feed self-play / training and the Train part of gate suites.
//! * Gate tool families are never used for training: they measure transfer to unseen task shapes.
//! * Guard families need no tools; they detect regressions in general competence (they are
//!   labelled `Split::Gate` because they are never trained on, and `needs_tools = false`).
//!
//! Prompts read like a user's request and never name a tool: the agent picks its own tools.
use super::verify::Check;
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
};

#[derive(Debug, Clone)]
pub struct Task {
    pub family: &'static str,
    pub prompt: String,
    /// Seeded workspace: relative path -> content.
    pub files: BTreeMap<String, String>,
    pub check: Check,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Split {
    Train,
    Gate,
}

#[derive(Debug, Clone)]
pub struct FamilyInfo {
    pub name: &'static str,
    pub split: Split,
    pub needs_tools: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    SelfPlay,
    Primary,
    Guard,
    Anchor,
}

type Build = fn(&mut StdRng) -> Task;

struct Family {
    name: &'static str,
    split: Split,
    needs_tools: bool,
    build: Build,
}

const fn tool(name: &'static str, split: Split, build: Build) -> Family {
    Family {
        name,
        split,
        needs_tools: true,
        build,
    }
}
const fn guard(name: &'static str, build: Build) -> Family {
    Family {
        name,
        split: Split::Gate,
        needs_tools: false,
        build,
    }
}

/// Definition order is the Anchor round-robin order: do not reorder casually, it changes the
/// frozen anchor mixture.
static FAMILIES: &[Family] = &[
    tool("read_line", Split::Train, read_line),
    tool("create_file", Split::Train, create_file),
    tool("replace_value", Split::Train, replace_value),
    tool("count_matches", Split::Train, count_matches),
    tool("list_by_ext", Split::Train, list_by_ext),
    tool("run_compute", Split::Train, run_compute),
    tool("fix_bug_py", Split::Train, fix_bug_py),
    tool("json_set", Split::Train, json_set),
    tool("append_line", Split::Train, append_line),
    tool("rename_symbol", Split::Train, rename_symbol),
    tool("csv_lookup", Split::Train, csv_lookup),
    tool("sum_two_files", Split::Gate, sum_two_files),
    tool("move_file", Split::Gate, move_file),
    tool("multi_file_edit", Split::Gate, multi_file_edit),
    tool("typo_hunt", Split::Gate, typo_hunt),
    tool("script_and_run", Split::Gate, script_and_run),
    guard("format_only", format_only),
    guard("arithmetic", arithmetic),
    guard("string_manip", string_manip),
    guard("list_extreme", list_extreme),
    guard("no_touch_files", no_touch_files),
    guard("unit_convert", unit_convert),
    guard("day_math", day_math),
    guard("direct_facts", direct_facts),
    // Append only: existing frozen anchor slots must not change.
    tool("config_precedence", Split::Train, config_precedence),
    tool("negative_search", Split::Train, negative_search),
    tool("semantic_validation", Split::Train, semantic_validation),
    tool("config_layers", Split::Gate, config_layers),
    tool("search_recovery", Split::Gate, search_recovery),
    tool("semantic_boundaries", Split::Gate, semantic_boundaries),
    tool("repository_contract", Split::Train, repository_contract),
    tool("repository_validation", Split::Gate, repository_validation),
];

pub fn families() -> Vec<FamilyInfo> {
    FAMILIES
        .iter()
        .map(|f| FamilyInfo {
            name: f.name,
            split: f.split,
            needs_tools: f.needs_tools,
        })
        .collect()
}

/// Deterministic in `(role, n, seed)`; prompts are pairwise distinct within one call.
pub fn generate(role: Role, n: usize, seed: u64) -> Vec<Task> {
    let train: Vec<&Family> = FAMILIES
        .iter()
        .filter(|f| f.needs_tools && f.split == Split::Train)
        .collect();
    let gate: Vec<&Family> = FAMILIES
        .iter()
        .filter(|f| f.needs_tools && f.split == Split::Gate)
        .collect();
    let guards: Vec<&Family> = FAMILIES.iter().filter(|f| !f.needs_tools).collect();
    let rotation = |len: usize| (mix(&[seed, 0xA11CE]) % len.max(1) as u64) as usize;
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(n);
    let tag = role as u64;
    match role {
        Role::SelfPlay => fill_round_robin(
            &train,
            rotation(train.len()),
            n,
            0,
            [seed, tag],
            &mut seen,
            &mut out,
        ),
        Role::Guard => fill_round_robin(
            &guards,
            rotation(guards.len()),
            n,
            0,
            [seed, tag],
            &mut seen,
            &mut out,
        ),
        Role::Anchor => {
            let all: Vec<&Family> = FAMILIES.iter().collect();
            fill_round_robin(&all, 0, n, 0, [seed, tag], &mut seen, &mut out);
        }
        Role::Primary => {
            let n_gate = (2 * n + 2) / 5;
            fill_round_robin(
                &train,
                rotation(train.len()),
                n - n_gate,
                0,
                [seed, tag],
                &mut seen,
                &mut out,
            );
            fill_round_robin(
                &gate,
                rotation(gate.len()),
                n_gate,
                n,
                [seed, tag],
                &mut seen,
                &mut out,
            );
            out.shuffle(&mut StdRng::seed_from_u64(mix(&[seed, tag, 0x5EED])));
        }
    }
    out
}
/// Deterministic self-play generation that allocates half the budget (at least one task per
/// requested family) to requested train families, then fills the remainder normally.
pub fn generate_targeted(role: Role, n: usize, seed: u64, priorities: &[String]) -> Vec<Task> {
    if role != Role::SelfPlay {
        return generate(role, n, seed);
    }

    let train: Vec<&Family> = FAMILIES
        .iter()
        .filter(|f| f.needs_tools && f.split == Split::Train)
        .collect();
    let prioritized: Vec<&Family> = priorities
        .iter()
        .filter_map(|name| train.iter().copied().find(|family| family.name == name))
        .fold(
            (Vec::new(), BTreeSet::new()),
            |(mut families, mut seen), family| {
                if seen.insert(family.name) {
                    families.push(family);
                }
                (families, seen)
            },
        )
        .0;
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(n);
    let targeted_count = n.min(prioritized.len().max(n / 2 + n % 2));
    let start =
        (mix(&[seed, Role::SelfPlay as u64, 0x7A26E7]) % prioritized.len().max(1) as u64) as usize;
    fill_round_robin(
        &prioritized,
        start,
        targeted_count,
        0,
        [seed, Role::SelfPlay as u64],
        &mut seen,
        &mut out,
    );
    let rotation = (mix(&[seed, 0xA11CE]) % train.len().max(1) as u64) as usize;
    fill_round_robin(
        &train,
        rotation,
        n.saturating_sub(out.len()),
        0,
        [seed, Role::SelfPlay as u64],
        &mut seen,
        &mut out,
    );
    out
}

/// Sealed transfer tests for the targeted failure families. Selection never depends on observed
/// gate scores or practice priorities, and these contracts cannot enter self-play.
pub fn generate_failure_holdout(n: usize, seed: u64) -> Vec<Task> {
    let families = FAMILIES
        .iter()
        .filter(|family| {
            matches!(
                family.name,
                "config_layers" | "search_recovery" | "semantic_boundaries"
            )
        })
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut tasks = Vec::with_capacity(n);
    fill_round_robin(
        &families,
        0,
        n,
        0,
        [seed, 0xFA11_401D],
        &mut seen,
        &mut tasks,
    );
    tasks
}

/// Generate `count` tasks cycling through `families` starting at `start`. Instance `i` depends on
/// `(seed, role, base + i, attempt)` and earlier prompts used for duplicate rejection.
fn fill_round_robin(
    families: &[&Family],
    start: usize,
    count: usize,
    base: usize,
    key: [u64; 2],
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Task>,
) {
    if families.is_empty() {
        return;
    }
    for i in 0..count {
        // A family whose instance space is exhausted hands the slot to the next one.
        'slot: for hop in 0..families.len() {
            let family = families[(start + i + hop) % families.len()];
            for attempt in 0..48u64 {
                let mut rng = StdRng::seed_from_u64(mix(&[
                    key[0],
                    key[1],
                    (base + i) as u64,
                    hop as u64,
                    attempt,
                ]));
                let task = (family.build)(&mut rng);
                if seen.insert(task.prompt.clone()) {
                    out.push(task);
                    break 'slot;
                }
            }
        }
    }
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn mix(parts: &[u64]) -> u64 {
    parts
        .iter()
        .fold(0x243F_6A88_85A3_08D3, |h, p| splitmix(h ^ splitmix(*p)))
}

// ---------------------------------------------------------------------------------------------
// Generation helpers

const WORDS: &[&str] = &[
    "amber", "anchor", "birch", "cactus", "canyon", "cedar", "cobalt", "comet", "copper", "coral",
    "crimson", "delta", "ember", "falcon", "fern", "flint", "forest", "garnet", "glacier",
    "harbor", "hazel", "indigo", "ivory", "jasper", "juniper", "kettle", "lantern", "lichen",
    "lotus", "maple", "marble", "meadow", "mesa", "onyx", "orchid", "otter", "pebble", "pine",
    "quartz", "raven", "ridge", "saffron", "sierra", "silver", "sparrow", "summit", "tundra",
    "umber", "velvet", "willow", "zephyr", "basalt", "thistle", "walnut", "yarrow", "bramble",
    "cinder", "dune", "fjord", "gale", "heron", "nickel", "poplar", "violet",
];
const NAMES: &[&str] = &[
    "Alice", "Bruno", "Carmen", "Dmitri", "Elena", "Farid", "Greta", "Hiro", "Imani", "Jonas",
    "Kavya", "Liam", "Mina", "Nadia", "Oscar", "Priya", "Quinn", "Rosa", "Sven", "Tariq", "Uma",
    "Viktor", "Wen", "Ximena", "Yusuf", "Zoe",
];
const SURNAMES: &[&str] = &[
    "Abara",
    "Brandt",
    "Castillo",
    "Dumont",
    "Eriksen",
    "Fontaine",
    "Grimaldi",
    "Haddad",
    "Ivanov",
    "Jensen",
    "Kowalski",
    "Lindqvist",
    "Moreau",
    "Novak",
    "Okafor",
    "Petrov",
    "Quintero",
    "Rossi",
    "Sato",
    "Tanaka",
];
const CITIES: &[&str] = &[
    "Lisbon", "Osaka", "Nairobi", "Oslo", "Lima", "Cairo", "Dublin", "Hanoi", "Quito", "Riga",
    "Turin", "Perth", "Bergen", "Kyoto", "Accra", "Seville",
];
const DIRS: &[&str] = &[
    "src", "docs", "data", "config", "notes", "reports", "assets", "logs", "scripts", "archive",
    "inputs", "exports",
];
const SUBDIRS: &[&str] = &[
    "alpha", "beta", "drafts", "team", "misc", "lib", "old", "staging", "shared", "q3", "backup",
    "current",
];

fn pick<T: Copy>(rng: &mut StdRng, xs: &[T]) -> T {
    xs[rng.random_range(0..xs.len())]
}

fn int(rng: &mut StdRng, lo: i64, hi: i64) -> i64 {
    rng.random_range(lo..=hi)
}

fn sample<T: Copy>(rng: &mut StdRng, xs: &[T], n: usize) -> Vec<T> {
    let mut v = xs.to_vec();
    v.shuffle(rng);
    v.truncate(n);
    v
}

fn fill(template: &str, kv: &[(&str, &str)]) -> String {
    kv.iter().fold(template.to_string(), |s, (k, v)| {
        s.replace(&format!("{{{k}}}"), v)
    })
}

fn rand_dir(rng: &mut StdRng) -> String {
    match rng.random_range(0..4) {
        0 => pick(rng, DIRS).to_string(),
        1 | 2 => format!("{}/{}", pick(rng, DIRS), pick(rng, SUBDIRS)),
        _ => format!(
            "{}/{}/{}",
            pick(rng, DIRS),
            pick(rng, SUBDIRS),
            pick(rng, WORDS)
        ),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn file_map(items: Vec<(String, String)>) -> BTreeMap<String, String> {
    items.into_iter().collect()
}

fn task(family: &'static str, prompt: String, files: Vec<(String, String)>, check: Check) -> Task {
    Task {
        family,
        prompt,
        files: file_map(files),
        check,
    }
}

fn all(checks: Vec<Check>) -> Check {
    Check::All { checks }
}

fn answer(expected: impl ToString) -> Check {
    Check::AnswerExact {
        expected: expected.to_string(),
    }
}

fn file_equals(path: &str, content: impl Into<String>) -> Check {
    Check::FileEquals {
        path: path.to_string(),
        content: content.into(),
    }
}

fn lines_text(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

// These failure-targeted families use only generated data. Train and held-out variants have
// distinct contracts and module namespaces; recorded user code and gate prompts are never inputs.
fn config_precedence(rng: &mut StdRng) -> Task {
    configuration_task(rng, false)
}
fn config_layers(rng: &mut StdRng) -> Task {
    configuration_task(rng, true)
}

fn configuration_task(rng: &mut StdRng, held_out: bool) -> Task {
    let family = if held_out {
        "config_layers"
    } else {
        "config_precedence"
    };
    let module = format!("{family}_{}", int(rng, 1000, 999999));
    let key = pick(rng, WORDS);
    let fallback = int(rng, 10, 99);
    let low = int(rng, 100, 199);
    let high = int(rng, 200, 299);
    let (contract, buggy) = if held_out {
        (
            "Layers are ordered highest priority first. Return the first layer containing the key. A present null explicitly clears the value; false, zero and empty strings also override. Only an absent key falls through to the fallback.",
            "    for layer in reversed(layers):\n        if key in layer:\n            return layer[key]\n    return fallback\n",
        )
    } else {
        (
            "Layers are ordered lowest priority first. Return the last non-null value for the key. Missing keys and null inherit; false, zero and empty strings are explicit overrides. If no layer supplies a value, return the fallback.",
            "    value = fallback\n    for layer in layers:\n        value = layer.get(key) or value\n    return value\n",
        )
    };
    let visible = json!([{key:low}, {key:0}]);
    let cases = vec![
        (visible.clone(), json!(if held_out { low } else { 0 })),
        (
            json!([{key:low}, {key:high}]),
            json!(if held_out { low } else { high }),
        ),
        (
            json!([{key:null}, {key:high}]),
            if held_out { Value::Null } else { json!(high) },
        ),
        (json!([{key:low}, {key:null}]), json!(low)),
        (json!([{key:false}]), json!(false)),
        (json!([{key:""}]), json!("")),
        (json!([{}, {"unrelated": high}]), json!(fallback)),
        (json!([]), json!(fallback)),
    ];
    let source =
        format!("def resolve(layers, key, fallback):\n    \"\"\"{contract}\"\"\"\n{buggy}");
    let probes = cases
        .iter()
        .map(|(layers, _)| {
            format!(
                "print(json.dumps(m.resolve(json.loads({:?}), {:?}, {fallback}), sort_keys=True))",
                layers.to_string(),
                key
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let expected = cases
        .iter()
        .map(|(_, expected)| expected.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    task(
        family,
        format!(
            "Fix resolve in {module}.py. {contract} For example, layers {visible}, key {key:?}, fallback {fallback}. Correct precedence must hold on other inputs too."
        ),
        vec![(format!("{module}.py"), source)],
        python_check(&module, &format!("import json\n{probes}"), expected),
    )
}

fn negative_search(rng: &mut StdRng) -> Task {
    search_task(rng, false)
}
fn search_recovery(rng: &mut StdRng) -> Task {
    search_task(rng, true)
}

fn search_task(rng: &mut StdRng, held_out: bool) -> Task {
    let family = if held_out {
        "search_recovery"
    } else {
        "negative_search"
    };
    let module = format!("{family}_{}", int(rng, 1000, 999999));
    let token = format!("{}{}", pick(rng, WORDS), int(rng, 100, 999));
    let missing = format!("absent_{}", int(rng, 1000, 9999));
    let root = format!("records_{}", int(rng, 1000, 9999));
    let text = format!("ordinary entry\n{token}\n");
    let (contract, pattern, buggy) = if held_out {
        (
            "Search recursively in .txt files only, with case-insensitive literal substring matching.",
            "**/*.txt",
            "    try:\n        return sorted(str(p.relative_to(root)) for p in root.glob('**/*') if p.is_file() and needle in p.read_text())\n    except OSError:\n        return []\n",
        )
    } else {
        (
            "Search only immediate .txt files (not subdirectories), with case-sensitive literal substring matching.",
            "*.txt",
            "    try:\n        return sorted(str(p.relative_to(root)) for p in root.glob('*.txt') if needle in p.read_text())\n    except OSError:\n        return []\n",
        )
    };
    let source = format!(
        "from pathlib import Path\n\ndef search(directory, needle):\n    root = Path(directory)\n{buggy}"
    );
    let matching = if held_out {
        vec!["entry.txt", "nested/entry.txt"]
    } else {
        vec!["entry.txt"]
    };
    let query = if held_out {
        token.to_uppercase()
    } else {
        token.clone()
    };
    let probes = format!(
        "import json\nprint(json.dumps(m.search({root:?}, {query:?})))\nprint(json.dumps(m.search({root:?}, {missing:?})))\ntry:\n    m.search({missing:?}, {query:?})\nexcept FileNotFoundError:\n    print(\"missing-root\")\nelse:\n    print(\"incorrect-empty-success\")\ntry:\n    m.search({file:?}, {query:?})\nexcept NotADirectoryError:\n    print(\"file-root\")\nelse:\n    print(\"incorrect-file-success\")",
        file = format!("{root}/entry.txt")
    );
    let files = vec![
        (format!("{module}.py"), source),
        (format!("{root}/entry.txt"), text.clone()),
        (format!("{root}/nested/entry.txt"), text.clone()),
        (format!("{root}/ignored.log"), text),
    ];
    let mut checks = vec![python_check(
        &module,
        &probes,
        format!(
            "{}\n[]\nmissing-root\nfile-root",
            serde_json::to_string(&matching)
                .unwrap()
                .replace("\",\"", "\", \"")
        ),
    )];
    // Searches must not manufacture success by deleting or rewriting their inputs.
    checks.extend(
        files
            .iter()
            .skip(1)
            .map(|(path, content)| file_equals(path, content.clone())),
    );
    task(
        family,
        format!(
            "Repair search in {module}.py. {contract} Return sorted relative paths. A valid search with no hits returns []; a missing root must raise FileNotFoundError and a file used as the root must raise NotADirectoryError. Do not swallow operational failures as empty results or change the input files. Try looking for {token:?} under {root}; the search pattern is {pattern:?}."
        ),
        files,
        all(checks),
    )
}

fn semantic_validation(rng: &mut StdRng) -> Task {
    boundary_task(rng, false)
}
fn semantic_boundaries(rng: &mut StdRng) -> Task {
    boundary_task(rng, true)
}

fn boundary_task(rng: &mut StdRng, held_out: bool) -> Task {
    let family = if held_out {
        "semantic_boundaries"
    } else {
        "semantic_validation"
    };
    let module = format!("{family}_{}", int(rng, 1000, 999999));
    let low = int(rng, -30, -1);
    let high = int(rng, 1, 30);
    let contract = if held_out {
        "lower-inclusive, upper-exclusive"
    } else {
        "inclusive at both ends"
    };
    let source = "def select(values, lower, upper):\n    return [value for value in values if lower < value < upper]\n".to_owned();
    let cases = [
        (vec![low - 1, low, low, 0, high, high + 1], low, high),
        (vec![high, low, high, low - 1], low, high),
        (vec![low, 0, high], high, low),
        (vec![0, 0, 1, -1], 0, 0),
        (vec![], low, high),
    ];
    let probes = cases
        .iter()
        .map(|(values, lo, hi)| format!("print(m.select({}, {lo}, {hi}))", py_list(values)))
        .collect::<Vec<_>>()
        .join("\n");
    let expected = cases
        .iter()
        .map(|(values, lo, hi)| {
            let selected = values
                .iter()
                .copied()
                .filter(|v| *v >= *lo && if held_out { *v < *hi } else { *v <= *hi })
                .collect::<Vec<_>>();
            py_list(&selected)
        })
        .collect::<Vec<_>>()
        .join("\n");
    task(
        family,
        format!(
            "The syntactically valid select function in {module}.py has a semantic bug. Keep values in the interval {contract}, preserving input order and duplicates. An inverted interval returns an empty list. Repair behavior, not just syntax; try bounds {low} and {high}, including values equal to each boundary."
        ),
        vec![(format!("{module}.py"), source)],
        python_check(&module, &probes, expected),
    )
}

fn python_check(module: &str, probes: &str, expected: String) -> Check {
    Check::Command {
        command: format!(
            "python3 -B - <<'DIAL_VERIFY'\nimport {module} as m\n{probes}\nDIAL_VERIFY"
        ),
        expect_stdout: Some(expected),
        expect_exit: 0,
    }
}

fn repair_targeted_task(task: &Task, ws: &Path) {
    let Some(path) = task.files.keys().find(|path| path.ends_with(".py")) else {
        return;
    };
    let source = match task.family {
        "config_precedence" => "def resolve(layers, key, fallback):\n    value = fallback\n    for layer in layers:\n        if key in layer and layer[key] is not None:\n            value = layer[key]\n    return value\n".to_owned(),
        "config_layers" => "def resolve(layers, key, fallback):\n    for layer in layers:\n        if key in layer:\n            return layer[key]\n    return fallback\n".to_owned(),
        "negative_search" | "search_recovery" => {
            let (pattern, condition) = if task.family == "negative_search" {
                ("*.txt", "needle in p.read_text()")
            } else { ("**/*.txt", "needle.casefold() in p.read_text().casefold()") };
            format!("from pathlib import Path\n\ndef search(directory, needle):\n    root = Path(directory)\n    if not root.exists():\n        raise FileNotFoundError(directory)\n    if not root.is_dir():\n        raise NotADirectoryError(directory)\n    return sorted(str(p.relative_to(root)) for p in root.glob({pattern:?}) if p.is_file() and {condition})\n")
        }
        "semantic_validation" => "def select(values, lower, upper):\n    return [v for v in values if lower <= v <= upper]\n".to_owned(),
        "semantic_boundaries" => "def select(values, lower, upper):\n    return [v for v in values if lower <= v < upper]\n".to_owned(),
        _ => return,
    };
    write_file(ws, path, &source);
}
// ---------------------------------------------------------------------------------------------
// TRAIN families

// Multi-module exercises require preserving a public wrapper and unrelated repository files.
fn repository_contract(rng: &mut StdRng) -> Task {
    let limit = int(rng, 10, 90);
    let readme = format!(
        "Public API: summarize(values). Accept values at least {limit}; None is missing, zero is a value.\n"
    );
    task("repository_contract",format!("Fix the package's filtering and summary bugs. Values equal to {limit} must be included, None ignored, and summarize must return count and total. Preserve the public function signatures and README. Run checks for boundary values and empty input."),vec![
        ("sample/__init__.py".into(),"from .api import summarize\n".into()),
        ("sample/core.py".into(),format!("def accepted(values):\n    return [v for v in values if v and v > {limit}]\n")),
        ("sample/api.py".into(),"from .core import accepted\ndef summarize(values):\n    return {'count': len(values), 'total': sum(accepted(values))}\n".into()),
        ("README.md".into(),readme.clone()),
    ],all(vec![file_equals("README.md",readme),
        Check::Command {command:format!("python3 -B -c \"from sample import summarize; assert summarize([None, {}, {limit}, {}]) == {{'count': 2, 'total': {}}}; assert summarize([]) == {{'count': 0, 'total': 0}}\"",limit-1,limit+3,2*limit+3),expect_stdout:None,expect_exit:0}]))
}
fn repository_validation(rng: &mut StdRng) -> Task {
    let limit = int(rng, 100, 999);
    let readme = format!(
        "parse_amount accepts integer amounts from zero through {limit}; booleans and strings are invalid.\n"
    );
    task("repository_validation",format!("Repair amount validation across this package: accept integers 0 through {limit} inclusive, reject bool and strings with ValueError, and preserve the service wrapper and documentation. Include boundary and invalid-input checks."),vec![
        ("ledger/__init__.py".into(),"".into()),
        ("ledger/amount.py".into(),format!("def parse_amount(value):\n    value = int(value)\n    if not 0 < value < {limit}:\n        raise ValueError('invalid amount')\n    return value\n")),
        ("ledger/service.py".into(),"from .amount import parse_amount\ndef total(values):\n    return sum(parse_amount(v) for v in values)\n".into()),
        ("README.md".into(),readme.clone()),
    ],all(vec![file_equals("ledger/service.py","from .amount import parse_amount\ndef total(values):\n    return sum(parse_amount(v) for v in values)\n"),file_equals("README.md",readme),
        Check::Command{command:format!("python3 -B - <<'PY'\nfrom ledger.service import total\nfrom ledger.amount import parse_amount\nassert total([0,{limit}]) == {limit}\nfor value in [True, False, '3', -1, {}]:\n    try: parse_amount(value)\n    except ValueError: pass\n    else: raise AssertionError('accepted invalid value')\nPY",limit+1),expect_stdout:None,expect_exit:0}]))
}

fn read_line(rng: &mut StdRng) -> Task {
    let n = rng.random_range(5..=16usize);
    let style = rng.random_range(0..3);
    let mut lines: Vec<String> = Vec::new();
    while lines.len() < n {
        let line = match style {
            0 => format!("{} {}", pick(rng, WORDS), int(rng, 100, 9999)),
            1 => format!("{}: {}", pick(rng, NAMES), pick(rng, CITIES)),
            _ => format!(
                "2025-{:02}-{:02} {} {}",
                int(rng, 1, 12),
                int(rng, 1, 28),
                pick(rng, WORDS),
                pick(rng, &["started", "finished", "skipped", "retried"])
            ),
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    let k = rng.random_range(1..=n);
    let path = join(
        &rand_dir(rng),
        &format!(
            "{}.{}",
            pick(rng, WORDS),
            pick(rng, &["txt", "log", "md", "csv"])
        ),
    );
    let prompt = fill(
        pick(
            rng,
            &[
                "What's on line {k} of {path}? Reply with only that line, exactly as written.",
                "Open {path} and tell me what line {k} says. Reply with only the text of that line.",
                "I need line number {k} from {path}. Give me only that line and nothing else.",
            ],
        ),
        &[("k", &k.to_string()), ("path", &path)],
    );
    task(
        "read_line",
        prompt,
        vec![(path, lines_text(&lines))],
        answer(&lines[k - 1]),
    )
}

fn create_file(rng: &mut StdRng) -> Task {
    let path = join(
        &rand_dir(rng),
        &format!(
            "{}.{}",
            pick(rng, WORDS),
            pick(rng, &["txt", "md", "cfg", "list"])
        ),
    );
    let n = rng.random_range(1..=4usize);
    let content = match rng.random_range(0..3) {
        0 => (0..n)
            .map(|_| {
                format!(
                    "{} {} {}",
                    pick(rng, WORDS),
                    pick(rng, WORDS),
                    int(rng, 1, 99)
                )
            })
            .collect::<Vec<_>>(),
        1 => (0..n)
            .map(|i| format!("{}_{}={}", pick(rng, WORDS), i, int(rng, 1, 500)))
            .collect(),
        _ => (0..n)
            .map(|_| format!("{}, {}", pick(rng, NAMES), pick(rng, CITIES)))
            .collect(),
    }
    .join("\n");
    let template = if n == 1 {
        pick(
            rng,
            &[
                "Create a new file {path} containing exactly this text and nothing else:\n\n{content}",
                "Make a file called {path} whose entire content is the single line: {content}",
            ],
        )
    } else {
        pick(
            rng,
            &[
                "Create a new file {path} with exactly the following content:\n\n{content}",
                "Please write the following lines into a new file named {path}, with nothing added:\n\n{content}",
            ],
        )
    };
    let prompt = fill(template, &[("path", &path), ("content", &content)]);
    // Sometimes the target directory already holds an unrelated file.
    let mut files = Vec::new();
    if rng.random_bool(0.4) {
        let dir = path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        files.push((
            join(&dir, "README"),
            format!("{} files live here.\n", pick(rng, WORDS)),
        ));
    }
    task("create_file", prompt, files, file_equals(&path, content))
}

#[derive(Clone, PartialEq)]
enum Val {
    Int(i64),
    Str(&'static str),
}

impl Val {
    fn plain(&self) -> String {
        match self {
            Val::Int(n) => n.to_string(),
            Val::Str(s) => s.to_string(),
        }
    }
}

const INT_KEYS: &[&str] = &[
    "timeout",
    "retries",
    "port",
    "max_connections",
    "batch_size",
    "cache_ttl",
    "workers",
    "page_size",
    "queue_size",
    "max_items",
];
const STR_KEYS: &[(&str, &[&str])] = &[
    ("mode", &["fast", "safe", "strict", "lenient"]),
    ("log_level", &["debug", "info", "warn", "error"]),
    ("theme", &["light", "dark", "solarized"]),
    ("region", &["eu-west", "us-east", "ap-south"]),
    ("format", &["json", "text", "csv"]),
];

/// Config syntaxes: 0 = python constants, 1 = env file, 2 = yaml, 3 = ini.
fn render_entry(style: u8, key: &str, v: &Val) -> String {
    let quoted = matches!(v, Val::Str(_));
    match style {
        0 if quoted => format!("{} = \"{}\"", key.to_uppercase(), v.plain()),
        0 => format!("{} = {}", key.to_uppercase(), v.plain()),
        1 => format!("{}={}", key, v.plain()),
        2 => format!("{}: {}", key, v.plain()),
        _ => format!("{} = {}", key, v.plain()),
    }
}

fn entry_key(style: u8, key: &str) -> String {
    if style == 0 {
        key.to_uppercase()
    } else {
        key.to_string()
    }
}

fn render_config(style: u8, entries: &[(&str, Val)]) -> String {
    let mut lines = vec![match style {
        0 => "# runtime settings".to_string(),
        1 => "# environment".to_string(),
        2 => "# configuration".to_string(),
        _ => "[settings]".to_string(),
    }];
    lines.extend(entries.iter().map(|(k, v)| render_entry(style, k, v)));
    lines_text(&lines)
}

fn config_name(rng: &mut StdRng, style: u8) -> String {
    match style {
        0 => pick(rng, &["settings.py", "config.py", "defaults.py"]).to_string(),
        1 => pick(rng, &["app.env", "service.env", "worker.env"]).to_string(),
        2 => pick(rng, &["config.yaml", "settings.yaml", "service.yml"]).to_string(),
        _ => pick(rng, &["settings.ini", "app.cfg", "service.conf"]).to_string(),
    }
}

/// `n` distinct settings (mostly numeric, sometimes enumerations).
fn random_entries(rng: &mut StdRng, n: usize) -> Vec<(&'static str, Val)> {
    let mut entries: Vec<(&'static str, Val)> = Vec::new();
    for key in sample(rng, INT_KEYS, n) {
        let hi = if key == "port" { 9999 } else { 500 };
        entries.push((key, Val::Int(int(rng, 2, hi))));
    }
    let n_strings = rng.random_range(0..=2);
    for (key, values) in sample(rng, STR_KEYS, n_strings) {
        entries.push((key, Val::Str(pick(rng, values))));
    }
    entries.shuffle(rng);
    entries
}

fn replace_value(rng: &mut StdRng) -> Task {
    let style = rng.random_range(0..4u8);
    let n = rng.random_range(3..=7usize);
    let mut entries = random_entries(rng, n);
    let target = rng.random_range(0..entries.len());
    let (key, old) = entries[target].clone();
    let new = match &old {
        Val::Int(o) => loop {
            let c = Val::Int(int(rng, 2, 9999));
            if c != Val::Int(*o) {
                break c;
            }
        },
        Val::Str(o) => {
            let values = STR_KEYS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .unwrap_or(&[]);
            Val::Str(values.iter().copied().find(|v| v != o).unwrap_or("other"))
        }
    };
    // A decoy sharing the old value defeats a blind global search-and-replace.
    if matches!(old, Val::Int(_)) && rng.random_bool(0.5) {
        if let Some(i) =
            (0..entries.len()).find(|&i| i != target && matches!(entries[i].1, Val::Int(_)))
        {
            entries[i].1 = old.clone();
        }
    }
    let path = join(&rand_dir(rng), &config_name(rng, style));
    let shown = entry_key(style, key);
    let prompt = fill(
        pick(
            rng,
            &[
                "In {path}, change {key} from {old} to {new}.",
                "Please update {path} so that {key} is {new} instead of {old}.",
                "Set {key} to {new} in {path}; it is currently {old}.",
                "The {key} setting in {path} needs to go from {old} to {new}. Only change that one setting.",
            ],
        ),
        &[
            ("path", &path),
            ("key", &shown),
            ("old", &old.plain()),
            ("new", &new.plain()),
        ],
    );
    let before = render_config(style, &entries);
    entries[target].1 = new;
    // The decoy keeps the old value: only the target changes.
    let after = render_config(style, &entries);
    task(
        "replace_value",
        prompt,
        vec![(path.clone(), before)],
        file_equals(&path, after),
    )
}

const MARKERS: &[&str] = &[
    "TODO",
    "FIXME",
    "HACK",
    "NOTE",
    "XXX",
    "DEPRECATED",
    "REVIEW",
    "BUG",
];

fn filler_line(rng: &mut StdRng) -> String {
    match rng.random_range(0..7) {
        0 => format!("def {}_{}(x):", pick(rng, WORDS), pick(rng, WORDS)),
        1 => format!("    return x + {}", int(rng, 1, 99)),
        2 => format!("{} = {}", pick(rng, WORDS), int(rng, 1, 999)),
        3 => format!("import {}", pick(rng, WORDS)),
        4 => format!("print(\"{}\")", pick(rng, WORDS)),
        5 => format!("    if {} > {}:", pick(rng, WORDS), int(rng, 1, 50)),
        _ => String::new(),
    }
}

fn count_matches(rng: &mut StdRng) -> Task {
    let marker = pick(rng, MARKERS);
    let root = rand_dir(rng);
    let subs = sample(rng, SUBDIRS, 2);
    let target_dir = join(&root, subs[0]);
    let other_dir = join(&root, subs[1]);
    let n_files = rng.random_range(2..=5usize);
    let stems = sample(rng, WORDS, n_files + 2);
    let mut files = Vec::new();
    let mut hits = 0usize;
    for (i, stem) in stems.iter().enumerate() {
        let counted = i < n_files;
        let dir = if counted {
            if i % 3 == 2 {
                join(&target_dir, "inner")
            } else {
                target_dir.clone()
            }
        } else {
            other_dir.clone()
        };
        let mut lines = Vec::new();
        for _ in 0..rng.random_range(6..=20usize) {
            let roll = rng.random_range(0..10);
            let line = if roll < 2 {
                if counted {
                    hits += 1;
                }
                match rng.random_range(0..3) {
                    0 => format!("    # {marker}: {} {}", pick(rng, WORDS), pick(rng, WORDS)),
                    1 => format!(
                        "{} = {}  # {marker} {}",
                        pick(rng, WORDS),
                        int(rng, 1, 99),
                        pick(rng, WORDS)
                    ),
                    // Two occurrences on one line still count as one matching line.
                    _ => format!("# {marker} {marker} {}", pick(rng, WORDS)),
                }
            } else if roll == 2 {
                // Look-alikes that must not be counted: another marker, or the wrong case.
                let other = pick(rng, MARKERS);
                if other == marker || rng.random_bool(0.5) {
                    format!("    # {}: {}", marker.to_lowercase(), pick(rng, WORDS))
                } else {
                    format!("    # {other}: {}", pick(rng, WORDS))
                }
            } else {
                filler_line(rng)
            };
            lines.push(line);
        }
        let ext = pick(rng, &["py", "txt"]);
        files.push((join(&dir, &format!("{stem}.{ext}")), lines_text(&lines)));
    }
    if hits == 0 {
        // Guarantee a non-trivial answer by planting one match in the first counted file.
        if let Some((_, content)) = files.first_mut() {
            content.push_str(&format!("# {marker}: revisit {}\n", pick(rng, WORDS)));
            hits = 1;
        }
    }
    let prompt = fill(
        pick(
            rng,
            &[
                "How many lines in the files under {dir} contain the exact text {marker}? The match is case-sensitive. Reply with only the number.",
                "Count the lines containing {marker} (case-sensitive) across everything inside {dir}, including subfolders. Reply with only the number.",
            ],
        ),
        &[("dir", &target_dir), ("marker", marker)],
    );
    task("count_matches", prompt, files, answer(hits))
}

fn list_by_ext(rng: &mut StdRng) -> Task {
    let ext = pick(
        rng,
        &["txt", "md", "csv", "json", "log", "py", "yaml", "ini"],
    );
    let names_variant = rng.random_bool(0.5);
    let n_target = if names_variant {
        rng.random_range(1..=6usize)
    } else if rng.random_bool(0.1) {
        0
    } else {
        rng.random_range(1..=6usize)
    };
    let n_other = rng.random_range(3..=9usize);
    let root = rand_dir(rng);
    let n_subs = rng.random_range(1..=3usize);
    let subs = sample(rng, SUBDIRS, n_subs);
    let stems = sample(rng, WORDS, n_target + n_other + 2);
    let others: Vec<&str> = ["txt", "md", "csv", "json", "log", "py", "yaml", "ini"]
        .into_iter()
        .filter(|e| *e != ext)
        .collect();
    let place = |rng: &mut StdRng| -> String {
        let at = rng.random_range(0..=subs.len());
        if at == subs.len() {
            root.clone()
        } else {
            join(&root, subs[at])
        }
    };
    let mut files = Vec::new();
    let mut names = Vec::new();
    for (i, stem) in stems.iter().enumerate() {
        let name = if i < n_target {
            let name = format!("{stem}.{ext}");
            names.push(name.clone());
            name
        } else if i < n_target + n_other {
            format!("{stem}.{}", pick(rng, &others))
        } else {
            // Backup copies end in .bak, so they are not matches.
            format!("{stem}.{ext}.bak")
        };
        files.push((
            join(&place(rng), &name),
            format!("{} {}\n", pick(rng, WORDS), int(rng, 1, 99)),
        ));
    }
    names.sort();
    let (prompt, expected) = if names_variant {
        let p = fill(
            "List the names of all the .{ext} files under {dir}, including every subfolder: file names only, no folders, in alphabetical order, separated by commas with no spaces. Reply with only that list.",
            &[("ext", ext), ("dir", &root)],
        );
        (p, names.join(","))
    } else {
        let p = fill(
            pick(
                rng,
                &[
                    "How many .{ext} files are there under {dir}, including all subfolders? Reply with only the number.",
                    "Count the files ending in .{ext} anywhere inside {dir}. Reply with only the number.",
                ],
            ),
            &[("ext", ext), ("dir", &root)],
        );
        (p, n_target.to_string())
    };
    task("list_by_ext", prompt, files, answer(expected))
}

fn is_prime(n: u64) -> bool {
    n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| n % d != 0)
}

/// Sum of decimal digits of the product of `factors` (little-endian big integer).
fn digit_sum_of_product(factors: impl Iterator<Item = u32>) -> u64 {
    let mut digits: Vec<u32> = vec![1];
    for f in factors {
        let mut carry = 0u32;
        for d in digits.iter_mut() {
            let v = *d * f + carry;
            *d = v % 10;
            carry = v / 10;
        }
        while carry > 0 {
            digits.push(carry % 10);
            carry /= 10;
        }
    }
    digits.iter().map(|d| *d as u64).sum()
}

fn run_compute(rng: &mut StdRng) -> Task {
    let tail = "Reply with only the number.";
    let (question, value): (String, u64) = match rng.random_range(0..7) {
        0 => {
            let n = int(rng, 50, 5000) as u64;
            (
                format!("Work out the sum of the squares of all whole numbers from 1 to {n}."),
                (1..=n).map(|i| i * i).sum(),
            )
        }
        1 => {
            let n = int(rng, 500, 30000) as u64;
            (
                format!("How many prime numbers are there below {n}?"),
                (2..n).filter(|&i| is_prime(i)).count() as u64,
            )
        }
        2 => {
            let n = int(rng, 30, 90) as usize;
            let (mut a, mut b) = (0u64, 1u64);
            for _ in 1..n {
                (a, b) = (b, a + b);
            }
            (
                format!("What is F({n}) if F(1) = 1, F(2) = 1 and F(k) = F(k-1) + F(k-2)?"),
                b,
            )
        }
        3 => {
            let k = int(rng, 40, 400) as usize;
            (
                format!("What is the sum of the decimal digits of 2^{k}?"),
                digit_sum_of_product(std::iter::repeat_n(2, k)),
            )
        }
        4 => {
            let n = int(rng, 15, 80) as u32;
            (
                format!("What is the sum of the digits of {n}! (n factorial)?"),
                digit_sum_of_product(1..=n),
            )
        }
        5 => {
            let (a, b) = (int(rng, 1, 500) as u64, int(rng, 1000, 20000) as u64);
            let (x, y) = (int(rng, 3, 9) as u64, int(rng, 11, 23) as u64);
            (
                format!(
                    "How many integers from {a} to {b} (inclusive) are divisible by {x} or by {y}?"
                ),
                (a..=b).filter(|i| i % x == 0 || i % y == 0).count() as u64,
            )
        }
        _ => {
            let start = int(rng, 20, 90000) as u64;
            let (mut n, mut steps) = (start, 0u64);
            while n != 1 {
                n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
                steps += 1;
            }
            (
                format!(
                    "Start from {start}; if the number is even halve it, otherwise replace it with 3n+1, and repeat until you reach 1. How many steps does that take?"
                ),
                steps,
            )
        }
    };
    task(
        "run_compute",
        format!("{question} {tail}"),
        Vec::new(),
        answer(value),
    )
}

// --- Python bug bank shared by fix_bug_py (bug present) and typo_hunt (correct bodies) ---

struct Bug {
    desc: &'static str,
    /// Function body lines; `lines[key]` is replaced by `buggy` / `fixed`.
    lines: &'static [&'static str],
    key: usize,
    buggy: &'static str,
    fixed: &'static str,
    /// Accumulator name used by the body (typo_hunt misspells its last use).
    var: &'static str,
    good: fn(&[i64]) -> i64,
    /// Whether the buggy version gives a different result on this input.
    exposes: fn(&[i64]) -> bool,
    input: fn(&mut StdRng) -> Vec<i64>,
}

fn list_of(rng: &mut StdRng, len: std::ops::RangeInclusive<usize>, lo: i64, hi: i64) -> Vec<i64> {
    let n = rng.random_range(len);
    (0..n).map(|_| int(rng, lo, hi)).collect()
}

static BUGS: &[Bug] = &[
    Bug {
        desc: "return the sum of all the numbers in the list",
        lines: &[
            "    total = 0",
            "    for i in range(len(xs)):",
            "        total += xs[i]",
            "    return total",
        ],
        key: 1,
        buggy: "    for i in range(len(xs) - 1):",
        fixed: "    for i in range(len(xs)):",
        var: "total",
        good: |xs| xs.iter().sum(),
        exposes: |xs| xs.last().is_some_and(|l| *l != 0),
        input: |rng| list_of(rng, 3..=8, 1, 60),
    },
    Bug {
        desc: "return the largest number in the list",
        lines: &[
            "    best = 0",
            "    for x in xs:",
            "        if x > best:",
            "            best = x",
            "    return best",
        ],
        key: 0,
        buggy: "    best = 0",
        fixed: "    best = xs[0]",
        var: "best",
        good: |xs| xs.iter().copied().max().unwrap_or(0),
        exposes: |xs| xs.iter().all(|x| *x < 0),
        input: |rng| list_of(rng, 3..=7, -90, -1),
    },
    Bug {
        desc: "count how many numbers in the list are even",
        lines: &[
            "    count = 0",
            "    for x in xs:",
            "        if x % 2 == 0:",
            "            count += 1",
            "    return count",
        ],
        key: 2,
        buggy: "        if x % 2 == 1:",
        fixed: "        if x % 2 == 0:",
        var: "count",
        good: |xs| xs.iter().filter(|x| x.rem_euclid(2) == 0).count() as i64,
        exposes: |xs| xs.iter().filter(|x| x.rem_euclid(2) == 0).count() * 2 != xs.len(),
        input: |rng| list_of(rng, 4..=9, 1, 50),
    },
    Bug {
        desc: "return the product of all the numbers in the list",
        lines: &[
            "    result = 1",
            "    for x in xs:",
            "        result *= x",
            "    return result",
        ],
        key: 0,
        buggy: "    result = 0",
        fixed: "    result = 1",
        var: "result",
        good: |xs| xs.iter().product(),
        exposes: |xs| xs.iter().product::<i64>() != 0,
        input: |rng| list_of(rng, 2..=6, 2, 9),
    },
    Bug {
        desc: "return the average of the numbers in the list, rounded down to a whole number",
        lines: &[
            "    total = 0",
            "    for x in xs:",
            "        total += x",
            "    return total // len(xs)",
        ],
        key: 3,
        buggy: "    return total / len(xs)",
        fixed: "    return total // len(xs)",
        var: "total",
        good: |xs| xs.iter().sum::<i64>().div_euclid(xs.len() as i64),
        exposes: |_| true,
        input: |rng| list_of(rng, 3..=8, 1, 99),
    },
    Bug {
        desc: "count how many numbers in the list are strictly positive",
        lines: &[
            "    count = 0",
            "    for x in xs:",
            "        if x > 0:",
            "            count += 1",
            "    return count",
        ],
        key: 2,
        buggy: "        if x >= 0:",
        fixed: "        if x > 0:",
        var: "count",
        good: |xs| xs.iter().filter(|x| **x > 0).count() as i64,
        exposes: |xs| xs.contains(&0),
        input: |rng| {
            let mut v = list_of(rng, 4..=8, -20, 20);
            let at = rng.random_range(0..v.len());
            v[at] = 0;
            v
        },
    },
];

const PY_MODULES: &[&str] = &[
    "stats", "calc", "helpers", "numbers", "utils", "summary", "metrics", "totals",
];
const FN_VERBS: &[&str] = &["compute", "calc", "get", "find", "summarize", "measure"];
const FN_NOUNS: &[&str] = &[
    "readings", "samples", "values", "counts", "entries", "scores", "orders",
];

fn bug_source(bug: &Bug, fname: &str, line: &str) -> String {
    let mut s = format!("def {fname}(xs):\n    \"\"\"Return: {}.\"\"\"\n", bug.desc);
    for (i, l) in bug.lines.iter().enumerate() {
        s.push_str(if i == bug.key { line } else { l });
        s.push('\n');
    }
    s
}

fn py_list(xs: &[i64]) -> String {
    format!(
        "[{}]",
        xs.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn fix_bug_py(rng: &mut StdRng) -> Task {
    let bug = &BUGS[rng.random_range(0..BUGS.len())];
    let module = pick(rng, PY_MODULES);
    let fname = format!("{}_{}", pick(rng, FN_VERBS), pick(rng, FN_NOUNS));
    let visible = loop {
        let v = (bug.input)(rng);
        if (bug.exposes)(&v) {
            break v;
        }
    };
    let visible_value = (bug.good)(&visible);
    let mut hidden: Vec<Vec<i64>> = (0..3).map(|_| (bug.input)(rng)).collect();
    // At least one probe must distinguish a constant-return implementation.
    while hidden.iter().all(|h| (bug.good)(h) == visible_value) {
        hidden[0] = (bug.input)(rng);
    }
    let mut source = bug_source(bug, &fname, bug.buggy);
    source.push_str(&format!(
        "\n\nif __name__ == \"__main__\":\n    print({fname}({}))\n",
        py_list(&visible)
    ));
    let mut expected = vec![visible_value.to_string()];
    expected.extend(hidden.iter().map(|h| (bug.good)(h).to_string()));
    // The agent only sees the visible input; the hidden ones stop a hard-coded print() passing.
    let probes = hidden
        .iter()
        .map(|h| format!("print({module}.{fname}({}))", py_list(h)))
        .collect::<Vec<_>>()
        .join("; ");
    let command = format!("python3 -B {module}.py && python3 -B -c 'import {module}; {probes}'");
    let prompt = fill(
        pick(
            rng,
            &[
                "Running `python3 {module}.py` should print {out} but it doesn't. {fname} is supposed to {desc}. Find the bug in {module}.py and fix it.",
                "There's a bug in {module}.py: {fname} should {desc}, yet `python3 {module}.py` gives the wrong result (it should print {out}). Please fix the script.",
                "{module}.py is giving wrong answers. The function {fname} ought to {desc}; the script should print {out} when run with python3. Can you fix it?",
            ],
        ),
        &[
            ("module", module),
            ("fname", &fname),
            ("desc", bug.desc),
            ("out", &expected[0]),
        ],
    );
    let check = Check::Command {
        command,
        expect_stdout: Some(expected.join("\n")),
        expect_exit: 0,
    };
    task(
        "fix_bug_py",
        prompt,
        vec![(format!("{module}.py"), source)],
        check,
    )
}

/// Typed config keys for json_set.
#[derive(Clone, Copy)]
enum Ty {
    Int,
    Bool,
    Str,
}

const JSON_SECTIONS: &[&str] = &[
    "server", "database", "cache", "logging", "auth", "queue", "limits", "features",
];
const JSON_KEYS: &[(&str, Ty)] = &[
    ("timeout", Ty::Int),
    ("retries", Ty::Int),
    ("port", Ty::Int),
    ("max_connections", Ty::Int),
    ("batch_size", Ty::Int),
    ("ttl", Ty::Int),
    ("workers", Ty::Int),
    ("debug", Ty::Bool),
    ("verbose", Ty::Bool),
    ("enabled", Ty::Bool),
    ("ssl", Ty::Bool),
    ("compress", Ty::Bool),
    ("host", Ty::Str),
    ("region", Ty::Str),
    ("level", Ty::Str),
    ("name", Ty::Str),
    ("mode", Ty::Str),
];

fn json_value(rng: &mut StdRng, key: &str, ty: Ty) -> Value {
    match ty {
        Ty::Int if key == "port" => json!(int(rng, 1024, 9999)),
        Ty::Int => json!(int(rng, 2, 999)),
        Ty::Bool => json!(rng.random_bool(0.5)),
        Ty::Str if key == "host" => {
            json!(format!("10.0.{}.{}", int(rng, 0, 255), int(rng, 1, 254)))
        }
        Ty::Str => json!(pick(rng, WORDS)),
    }
}

fn json_set(rng: &mut StdRng) -> Task {
    let n_sections = rng.random_range(2..=4usize);
    let sections = sample(rng, JSON_SECTIONS, n_sections);
    let mut config: Vec<(&str, Vec<(&str, Ty, Value)>)> = Vec::new();
    for s in &sections {
        let n_keys = rng.random_range(2..=4usize);
        let keys = sample(rng, JSON_KEYS, n_keys);
        config.push((
            s,
            keys.into_iter()
                .map(|(k, ty)| (k, ty, json_value(rng, k, ty)))
                .collect(),
        ));
    }
    let at = rng.random_range(0..config.len());
    let add_new = rng.random_bool(0.3);
    let section = config[at].0;
    let (leaf, new, prompt_template) = if add_new {
        let fresh: Vec<(&str, Ty)> = JSON_KEYS
            .iter()
            .copied()
            .filter(|(k, _)| !config[at].1.iter().any(|(e, ..)| e == k))
            .collect();
        let (key, ty) = pick(rng, &fresh);
        let value = json_value(rng, key, ty);
        config[at].1.push((key, ty, Value::Null)); // placeholder replaced below
        (
            config[at].1.len() - 1,
            value,
            "Add a {key} setting with the value {value} to the {section} section of {path}.",
        )
    } else {
        let leaf = rng.random_range(0..config[at].1.len());
        let (key, ty, old) = config[at].1[leaf].clone();
        let value = loop {
            let c = json_value(rng, key, ty);
            if c != old {
                break c;
            }
        };
        (
            leaf,
            value,
            pick(
                rng,
                &[
                    "In {path}, set {section}.{key} to {value}.",
                    "Update {path}: {section}.{key} should be {value}.",
                ],
            ),
        )
    };
    let key = config[at].1[leaf].0;
    let render = |config: &[(&str, Vec<(&str, Ty, Value)>)], skip_new: bool| -> Value {
        let mut root = serde_json::Map::new();
        for (s, entries) in config {
            let mut m = serde_json::Map::new();
            for (k, _, v) in entries {
                if skip_new && v.is_null() {
                    continue;
                }
                m.insert(k.to_string(), v.clone());
            }
            root.insert(s.to_string(), Value::Object(m));
        }
        Value::Object(root)
    };
    let before = render(&config, true);
    config[at].1[leaf].2 = new.clone();
    let after = render(&config, false);
    let path = join(
        &rand_dir(rng),
        &format!(
            "{}.json",
            pick(rng, &["settings", "app", "service", "config"])
        ),
    );
    let shown = match &new {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    };
    let prompt = fill(
        prompt_template,
        &[
            ("path", &path),
            ("section", section),
            ("key", key),
            ("value", &shown),
        ],
    );
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    let mut checks = Vec::new();
    if let Value::Object(root) = &after {
        for (s, section) in root {
            if let Value::Object(section) = section {
                for (k, v) in section {
                    checks.push(Check::JsonFile {
                        path: path.clone(),
                        pointer: format!("/{s}/{k}"),
                        expected: v.clone(),
                    });
                }
            }
        }
    }
    task(
        "json_set",
        prompt,
        vec![(path, pretty(&before))],
        all(checks),
    )
}

fn append_line(rng: &mut StdRng) -> Task {
    let (path, mut lines, new): (String, Vec<String>, String) = match rng.random_range(0..4) {
        0 => {
            let n_items = rng.random_range(4..=8usize);
            let items = sample(rng, WORDS, n_items);
            let lines = items[1..]
                .iter()
                .map(|w| format!("- {w} {}", pick(rng, &["review", "ship", "test", "draft"])))
                .collect();
            (
                join(&rand_dir(rng), "todo.txt"),
                lines,
                format!("- {} {}", items[0], pick(rng, &["clean up", "archive"])),
            )
        }
        1 => {
            let lines = (0..rng.random_range(3..=7usize))
                .map(|i| format!("10.0.{}.{} {}", i + 1, int(rng, 2, 250), pick(rng, WORDS)))
                .collect();
            (
                join(&rand_dir(rng), "hosts.txt"),
                lines,
                format!("10.0.99.{} {}", int(rng, 2, 250), pick(rng, WORDS)),
            )
        }
        2 => {
            let n_pkgs = rng.random_range(4..=7usize);
            let pkgs = sample(rng, WORDS, n_pkgs);
            let lines = pkgs[1..]
                .iter()
                .map(|w| {
                    format!(
                        "{w}=={}.{}.{}",
                        int(rng, 0, 5),
                        int(rng, 0, 20),
                        int(rng, 0, 9)
                    )
                })
                .collect();
            (
                join(&rand_dir(rng), "requirements.txt"),
                lines,
                format!(
                    "{}=={}.{}.{}",
                    pkgs[0],
                    int(rng, 0, 5),
                    int(rng, 0, 20),
                    int(rng, 0, 9)
                ),
            )
        }
        _ => {
            let lines = (0..rng.random_range(3..=7usize))
                .map(|i| {
                    format!(
                        "2025-0{}-{:02} {} deployed",
                        i + 1,
                        int(rng, 1, 28),
                        pick(rng, WORDS)
                    )
                })
                .collect();
            (
                join(&rand_dir(rng), "changelog.txt"),
                lines,
                format!(
                    "2025-09-{:02} {} deployed",
                    int(rng, 1, 28),
                    pick(rng, WORDS)
                ),
            )
        }
    };
    let prompt = fill(
        pick(
            rng,
            &[
                "Append this line to the end of {path}, keeping everything else as it is:\n\n{line}",
                "Add one new last line to {path} reading exactly:\n\n{line}",
                "Please put the following line at the bottom of {path} without touching the other lines:\n\n{line}",
            ],
        ),
        &[("path", &path), ("line", &new)],
    );
    let original = lines_text(&lines);
    lines.push(new);
    task(
        "append_line",
        prompt,
        vec![(path.clone(), original)],
        file_equals(&path, lines_text(&lines)),
    )
}

const RENAME_VERBS: &[&str] = &[
    "scale",
    "normalize",
    "adjust",
    "convert",
    "expand",
    "shift",
    "rescale",
    "refine",
];

fn rename_symbol(rng: &mut StdRng) -> Task {
    let verbs = sample(rng, RENAME_VERBS, 2);
    let old = format!("{}_values", verbs[0]);
    let new = format!("{}_values", verbs[1]);
    let k = int(rng, 2, 9);
    let data: Vec<String> = (0..3).map(|_| int(rng, 1, 40).to_string()).collect();
    let data = data.join(", ");
    // `{o}` is the renamed symbol, `{lit}` the identically-prefixed helper that must stay.
    let template = match rng.random_range(0..2) {
        0 => format!(
            "def {{o}}(values):\n    return [v * {k} for v in values]\n\n\ndef {{lit}}_total(values):\n    return sum({{o}}(values))\n\n\ndef main():\n    data = [{data}]\n    print({{o}}(data))\n    print({{lit}}_total(data))\n\n\nif __name__ == \"__main__\":\n    main()\n"
        ),
        _ => format!(
            "def {{o}}(items, offset=0):\n    out = []\n    for item in items:\n        out.append(item * {k} + offset)\n    return out\n\n\ndef {{lit}}_checked(items):\n    result = {{o}}(items)\n    assert len(result) == len(items)\n    return result\n\n\nprint({{o}}([{data}], 1))\nprint({{lit}}_checked([{data}]))\nprint(len({{o}}([])))\n"
        ),
    };
    let render = |name: &str| template.replace("{o}", name).replace("{lit}", &old);
    let path = join(&rand_dir(rng), &format!("{}.py", pick(rng, PY_MODULES)));
    let prompt = fill(
        pick(
            rng,
            &[
                "In {path}, rename the function {old} to {new} everywhere: its definition and every call to it. Other functions, even ones with a similar name, must keep their names.",
                "Please rename {old} to {new} throughout {path} (the definition and all of its uses). Leave every other identifier alone.",
            ],
        ),
        &[("path", &path), ("old", &old), ("new", &new)],
    );
    task(
        "rename_symbol",
        prompt,
        vec![(path.clone(), render(&old))],
        file_equals(&path, render(&new)),
    )
}

fn csv_lookup(rng: &mut StdRng) -> Task {
    let n = rng.random_range(8..=25usize);
    let mut people: BTreeSet<String> = BTreeSet::new();
    while people.len() < n {
        people.insert(format!("{} {}", pick(rng, NAMES), pick(rng, SURNAMES)));
    }
    let mut people: Vec<String> = people.into_iter().collect();
    people.shuffle(rng);
    let mut salaries: BTreeSet<i64> = BTreeSet::new();
    while salaries.len() < n {
        salaries.insert(int(rng, 30, 140) * 1000);
    }
    let mut salaries: Vec<i64> = salaries.into_iter().collect();
    salaries.shuffle(rng);
    let depts = sample(
        rng,
        &[
            "Sales",
            "Support",
            "Research",
            "Finance",
            "Legal",
            "Logistics",
        ],
        4,
    );
    let rows: Vec<(i64, String, &str, &str, i64)> = (0..n)
        .map(|i| {
            (
                1000 + i as i64 * 7 + int(rng, 0, 6),
                people[i].clone(),
                depts[i % depts.len()],
                pick(rng, CITIES),
                salaries[i],
            )
        })
        .collect();
    let mut lines = vec!["id,name,dept,city,salary".to_string()];
    lines.extend(
        rows.iter()
            .map(|r| format!("{},{},{},{},{}", r.0, r.1, r.2, r.3, r.4)),
    );
    let path = join(
        &rand_dir(rng),
        &format!(
            "{}.csv",
            pick(rng, &["staff", "employees", "roster", "people"])
        ),
    );
    let (prompt, expected) = match rng.random_range(0..3) {
        0 => {
            let row = &rows[rng.random_range(0..rows.len())];
            let (col, value) = match rng.random_range(0..3) {
                0 => ("dept", row.2.to_string()),
                1 => ("city", row.3.to_string()),
                _ => ("salary", row.4.to_string()),
            };
            (
                format!(
                    "In {path}, what is the {col} of {}? Reply with only the value.",
                    row.1
                ),
                value,
            )
        }
        1 => {
            let top = rows
                .iter()
                .max_by_key(|r| r.4)
                .map(|r| r.1.clone())
                .unwrap_or_default();
            (
                format!("Who has the highest salary in {path}? Reply with only the full name."),
                top,
            )
        }
        _ => {
            let dept = pick(rng, &depts);
            let total: i64 = rows.iter().filter(|r| r.2 == dept).map(|r| r.4).sum();
            (
                format!(
                    "What is the combined salary of everyone in the {dept} department according to {path}? Reply with only the number."
                ),
                total.to_string(),
            )
        }
    };
    task(
        "csv_lookup",
        prompt,
        vec![(path, lines_text(&lines))],
        answer(expected),
    )
}

// ---------------------------------------------------------------------------------------------
// GATE families (never trained on)

fn sum_two_files(rng: &mut StdRng) -> Task {
    let (a, b) = (int(rng, 2, 9999), int(rng, 2, 9999));
    let stems = sample(rng, WORDS, 3);
    let (p1, p2) = (
        join(&rand_dir(rng), &format!("{}.txt", stems[0])),
        join(&rand_dir(rng), &format!("{}.txt", stems[1])),
    );
    let out = join(&rand_dir(rng), &format!("{}.txt", stems[2]));
    let labelled = rng.random_bool(0.5);
    let (c1, c2) = if labelled {
        (
            format!("{}: {a}\n", pick(rng, WORDS)),
            format!("{}: {b}\n", pick(rng, WORDS)),
        )
    } else {
        (format!("{a}\n"), format!("{b}\n"))
    };
    let (op, value) = match rng.random_range(0..3) {
        0 => ("add them together", a + b),
        1 => ("subtract the second from the first", a - b),
        _ => ("multiply them", a * b),
    };
    let prompt = fill(
        pick(
            rng,
            &[
                "{p1} and {p2} each contain one number. Read them, {op}, and write the result to {out} as just the number.",
                "Take the number stored in {p1} and the one stored in {p2}; {op} and save the result in a new file {out} (the number only).",
            ],
        ),
        &[("p1", &p1), ("p2", &p2), ("out", &out), ("op", op)],
    );
    let check = all(vec![
        file_equals(&out, value.to_string()),
        file_equals(&p1, c1.clone()),
        file_equals(&p2, c2.clone()),
    ]);
    task("sum_two_files", prompt, vec![(p1, c1), (p2, c2)], check)
}

fn move_file(rng: &mut StdRng) -> Task {
    let stems = sample(rng, WORDS, 5);
    let ext = pick(rng, &["txt", "md", "csv", "log"]);
    let src_dir = rand_dir(rng);
    let name = format!("{}.{ext}", stems[0]);
    let src = join(&src_dir, &name);
    let content = lines_text(
        &(0..rng.random_range(2..=5usize))
            .map(|_| format!("{} {}", pick(rng, WORDS), int(rng, 1, 999)))
            .collect::<Vec<_>>(),
    );
    let mut dst_dir = rand_dir(rng);
    while dst_dir == src_dir {
        dst_dir = rand_dir(rng);
    }
    let rename = rng.random_bool(0.4);
    let dst_name = if rename {
        format!("{}.{ext}", stems[1])
    } else {
        name.clone()
    };
    let dst = join(&dst_dir, &dst_name);
    // Siblings in the source folder must stay exactly where and what they are.
    let mut files = vec![(src.clone(), content.clone())];
    let mut checks = vec![
        Check::FileAbsent { path: src.clone() },
        file_equals(&dst, content.clone()),
    ];
    for stem in &stems[2..2 + rng.random_range(1..=3usize)] {
        let sibling = join(&src_dir, &format!("{stem}.{ext}"));
        let text = format!("{} {}\n", pick(rng, WORDS), int(rng, 1, 999));
        checks.push(file_equals(&sibling, text.clone()));
        files.push((sibling, text));
    }
    let prompt = if rename {
        fill(
            "Move {src} to {dst}, renaming it in the process. The destination folder may not exist yet, and the original must be gone afterwards.",
            &[("src", &src), ("dst", &dst)],
        )
    } else {
        fill(
            "Move the file {src} into the folder {dir} (create the folder if it is missing). It should no longer be at its old location.",
            &[("src", &src), ("dir", &dst_dir)],
        )
    };
    task("move_file", prompt, files, all(checks))
}

fn multi_file_edit(rng: &mut StdRng) -> Task {
    let key = pick(rng, INT_KEYS);
    let (s1, s2) = {
        let a = rng.random_range(0..4u8);
        (a, (a + rng.random_range(1..4u8)) % 4)
    };
    let old = int(rng, 2, 9999);
    let new = loop {
        let c = int(rng, 2, 9999);
        if c != old {
            break c;
        }
    };
    let build = |style: u8, value: i64, rng: &mut StdRng| -> String {
        let others: Vec<&str> = INT_KEYS.iter().copied().filter(|k| *k != key).collect();
        let n_entries = rng.random_range(2..=4usize);
        let mut entries: Vec<(&str, Val)> = sample(rng, &others, n_entries)
            .into_iter()
            .map(|k| (k, Val::Int(int(rng, 2, 400))))
            .collect();
        entries.insert(rng.random_range(0..=entries.len()), (key, Val::Int(value)));
        render_config(style, &entries)
    };
    // Rendering with a shared rng state: the "after" text must equal the "before" text except for
    // the value, so render once and substitute on the exact line instead of re-sampling.
    let p1 = join(&rand_dir(rng), &config_name(rng, s1));
    let mut p2 = join(&rand_dir(rng), &config_name(rng, s2));
    while p2 == p1 {
        p2 = join(&rand_dir(rng), &config_name(rng, s2));
    }
    let (t1, t2) = (build(s1, old, rng), build(s2, old, rng));
    let swap = |text: &str, style: u8| {
        text.replace(
            &render_entry(style, key, &Val::Int(old)),
            &render_entry(style, key, &Val::Int(new)),
        )
    };
    // A third, unrelated file that must not change.
    let other_dir = rand_dir(rng);
    let s3 = rng.random_range(0..4u8);
    let p3 = join(&other_dir, &config_name(rng, s3));
    let t3 = lines_text(&[
        format!("# {} notes", pick(rng, WORDS)),
        format!("owner: {}", pick(rng, NAMES)),
    ]);
    let shown1 = entry_key(s1, key);
    let shown2 = entry_key(s2, key);
    let shown = if shown1 == shown2 {
        shown1.clone()
    } else {
        format!("{shown1} ({p1}) / {shown2} ({p2})")
    };
    let prompt = fill(
        pick(
            rng,
            &[
                "The {key} value is duplicated in {p1} and {p2}. Change it from {old} to {new} in both files and touch nothing else.",
                "We are changing {key} from {old} to {new}. It is set in both {p1} and {p2}; update both.",
            ],
        ),
        &[
            ("key", &shown),
            ("p1", &p1),
            ("p2", &p2),
            ("old", &old.to_string()),
            ("new", &new.to_string()),
        ],
    );
    let mut files = vec![(p1.clone(), t1.clone()), (p2.clone(), t2.clone())];
    let mut checks = vec![
        file_equals(&p1, swap(&t1, s1)),
        file_equals(&p2, swap(&t2, s2)),
    ];
    if p3 != p1 && p3 != p2 {
        checks.push(file_equals(&p3, t3.clone()));
        files.push((p3, t3));
    }
    task("multi_file_edit", prompt, files, all(checks))
}

fn typo(rng: &mut StdRng, word: &str, source: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    for _ in 0..24 {
        let at = rng.random_range(1..chars.len() - 1);
        let mut c = chars.clone();
        if rng.random_bool(0.5) && c[at] != c[at + 1] {
            c.swap(at, at + 1);
        } else {
            c.remove(at);
        }
        let candidate: String = c.into_iter().collect();
        if candidate != word && !source.contains(&candidate) {
            return candidate;
        }
    }
    format!("{word}e")
}

fn typo_hunt(rng: &mut StdRng) -> Task {
    let modules = sample(rng, PY_MODULES, 3);
    let main_name = pick(rng, &["main.py", "run.py", "app.py"]);
    let bad = rng.random_range(0..3usize);
    let mut files = Vec::new();
    let mut checks = Vec::new();
    let (mut imports, mut calls, mut expected) = (Vec::new(), Vec::new(), Vec::new());
    let mut seen_fns = BTreeSet::new();
    let dir = pick(rng, &["", "app", "pkg"]);
    for (i, module) in modules.iter().enumerate() {
        let bug = &BUGS[rng.random_range(0..BUGS.len())];
        let fname = loop {
            let f = format!("{}_{}", pick(rng, FN_VERBS), pick(rng, FN_NOUNS));
            if seen_fns.insert(f.clone()) {
                break f;
            }
        };
        let correct = bug_source(bug, &fname, bug.fixed);
        let input = (bug.input)(rng);
        imports.push(format!("from {module} import {fname}"));
        calls.push(format!("print({fname}({}))", py_list(&input)));
        expected.push((bug.good)(&input).to_string());
        let path = join(dir, &format!("{module}.py"));
        if i == bad {
            // Misspell the accumulator at its final use (the `return`), leaving it correct elsewhere.
            let wrong = typo(rng, bug.var, &correct);
            let at = correct.rfind(bug.var).unwrap_or(0);
            let broken = format!(
                "{}{}{}",
                &correct[..at],
                wrong,
                &correct[at + bug.var.len()..]
            );
            checks.push(file_equals(&path, correct));
            files.push((path, broken));
        } else {
            checks.push(file_equals(&path, correct.clone()));
            files.push((path, correct));
        }
    }
    let main_src = format!("{}\n\n\n{}\n", imports.join("\n"), calls.join("\n"));
    let main_path = join(dir, main_name);
    checks.push(file_equals(&main_path, main_src.clone()));
    checks.insert(
        0,
        Check::Command {
            command: format!(
                "cd {} && python3 -B {main_name}",
                if dir.is_empty() { "." } else { dir }
            ),
            expect_stdout: Some(expected.join("\n")),
            expect_exit: 0,
        },
    );
    let run = if dir.is_empty() {
        format!("python3 {main_name}")
    } else {
        format!("(cd {dir} && python3 {main_name})")
    };
    files.push((main_path, main_src));
    let prompt = fill(
        pick(
            rng,
            &[
                "Running `{run}` crashes with a NameError. Find the misspelled name and fix it. Don't change anything else.",
                "`{run}` fails with a NameError because of a typo somewhere in the project. Locate it and correct that one identifier; leave the rest of the code as is.",
            ],
        ),
        &[("run", &run)],
    );
    task("typo_hunt", prompt, files, all(checks))
}

struct ScriptKind {
    phrase: &'static str,
    py: &'static str,
    f: fn(&[i64]) -> i64,
}

static SCRIPT_KINDS: &[ScriptKind] = &[
    ScriptKind {
        phrase: "the sum of the squares of the numbers",
        py: "print(sum(n * n for n in nums))",
        f: |x| x.iter().map(|n| n * n).sum(),
    },
    ScriptKind {
        phrase: "how many of the numbers are greater than 50",
        py: "print(sum(1 for n in nums if n > 50))",
        f: |x| x.iter().filter(|n| **n > 50).count() as i64,
    },
    ScriptKind {
        phrase: "the difference between the largest and the smallest number",
        py: "print(max(nums) - min(nums))",
        f: |x| x.iter().max().unwrap_or(&0) - x.iter().min().unwrap_or(&0),
    },
    ScriptKind {
        phrase: "the sum of the even numbers",
        py: "print(sum(n for n in nums if n % 2 == 0))",
        f: |x| x.iter().filter(|n| *n % 2 == 0).sum(),
    },
    ScriptKind {
        phrase: "the sum of every number multiplied by its position in the file, counting positions from 1",
        py: "print(sum(i * n for i, n in enumerate(nums, 1)))",
        f: |x| x.iter().enumerate().map(|(i, n)| (i as i64 + 1) * n).sum(),
    },
];

fn script_and_run(rng: &mut StdRng) -> Task {
    let kind = &SCRIPT_KINDS[rng.random_range(0..SCRIPT_KINDS.len())];
    let nums = |rng: &mut StdRng| list_of(rng, 5..=12, 1, 99);
    let data = nums(rng);
    let script = join(
        &pick(rng, &["scripts", "tools_py", "bin", "work"]).replace("tools_py", "utils"),
        &format!(
            "{}.py",
            pick(rng, &["report", "summarize", "analyze", "stats"])
        ),
    );
    let data_path = join(
        &pick(rng, &["data", "inputs", "samples"]).to_string(),
        &format!("{}.txt", pick(rng, WORDS)),
    );
    let value = (kind.f)(&data);
    // Verify the original input too, then swap in a probe with a different result. Restore the
    // data even when the script fails; a missing backup or failed restoration must fail the check.
    let probe = loop {
        let probe = nums(rng);
        if (kind.f)(&probe) != value {
            break probe;
        }
    };
    let swapped = probe
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let command = format!(
        "cp -f {data_path} {data_path}.orig && {{ python3 -B {script} && printf '%s\\n' {swapped} > {data_path} && python3 -B {script}; rc=$?; mv -f {data_path}.orig {data_path} || exit $?; exit $rc; }}"
    );
    let prompt = fill(
        pick(
            rng,
            &[
                "{data} holds some whole numbers, one per line. Write a Python script at {script} that prints {phrase}, run it, and reply with only the number it prints.",
                "Create a Python script {script} that reads the numbers from {data} (one per line) and prints {phrase}. Run it and reply with only its output. The script has to stay in place when you're done.",
            ],
        ),
        &[
            ("data", &data_path),
            ("script", &script),
            ("phrase", kind.phrase),
        ],
    );
    let data_text = lines_text(&data.iter().map(|n| n.to_string()).collect::<Vec<_>>());
    let files = vec![(data_path.clone(), data_text.clone())];
    let check = all(vec![
        answer(value),
        Check::Command {
            command,
            expect_stdout: Some(format!("{value}\n{}", (kind.f)(&probe))),
            expect_exit: 0,
        },
        file_equals(&data_path, data_text),
    ]);
    task("script_and_run", prompt, files, check)
}

// ---------------------------------------------------------------------------------------------
// GUARD families: no tools needed

fn format_only(rng: &mut StdRng) -> Task {
    let w = pick(rng, WORDS);
    let n = int(rng, 2, 9999);
    let (prompt, expected) = match rng.random_range(0..6) {
        0 => (
            format!("Reply with only the word {w} in capital letters."),
            w.to_uppercase(),
        ),
        1 => (
            format!("Reply with only the word {w} written backwards."),
            w.chars().rev().collect(),
        ),
        2 => {
            let other = pick(rng, NAMES);
            (
                format!(
                    "Reply with exactly these two words joined by a single hyphen and nothing else: {w} {other}"
                ),
                format!("{w}-{other}"),
            )
        }
        3 => (
            format!("Is {n} an even number? Reply with only yes or no, in lowercase."),
            if n % 2 == 0 {
                "yes".into()
            } else {
                "no".into()
            },
        ),
        4 => (
            format!("Reply with only the first letter of the word {w}, in uppercase."),
            w[..1].to_uppercase(),
        ),
        _ => (
            format!("Reply with only the number {n} followed directly by a percent sign."),
            format!("{n}%"),
        ),
    };
    task("format_only", prompt, Vec::new(), answer(expected))
}

fn arithmetic(rng: &mut StdRng) -> Task {
    let (a, b, c) = (int(rng, 2, 99), int(rng, 2, 99), int(rng, 2, 40));
    let (question, value) = match rng.random_range(0..6) {
        0 => (format!("What is {} + {}?", a * 7, b * 13), a * 7 + b * 13),
        1 => (format!("What is {} - {}?", a * 11, b * 3), a * 11 - b * 3),
        2 => (format!("What is {a} * {b}?"), a * b),
        3 => (format!("What is {a} * {b} + {c}?"), a * b + c),
        4 => (format!("What is ({a} + {b}) * {c}?"), (a + b) * c),
        _ => (
            format!(
                "A box holds {a} pens and {c} boxes are delivered, then {b} pens are taken out. How many pens are left?"
            ),
            a * c - b,
        ),
    };
    task(
        "arithmetic",
        format!("{question} Reply with only the number."),
        Vec::new(),
        answer(value),
    )
}

fn string_manip(rng: &mut StdRng) -> Task {
    // A sentence with a unique longest word, so every question has exactly one answer.
    let words = loop {
        let n_words = rng.random_range(4..=7usize);
        let w = sample(rng, WORDS, n_words);
        let longest = w.iter().map(|x| x.len()).max().unwrap_or(0);
        if w.iter().filter(|x| x.len() == longest).count() == 1 {
            break w;
        }
    };
    let text = words.join(" ");
    let (question, expected) = match rng.random_range(0..8) {
        0 => (
            format!("How many words are in \"{text}\"?"),
            words.len().to_string(),
        ),
        1 => (
            format!("How many vowels (a, e, i, o, u) are in \"{text}\"?"),
            text.chars()
                .filter(|c| "aeiou".contains(*c))
                .count()
                .to_string(),
        ),
        2 => (
            format!("Which is the longest word in \"{text}\"?"),
            words
                .iter()
                .max_by_key(|w| w.len())
                .map(|w| w.to_string())
                .unwrap_or_default(),
        ),
        3 => (
            format!(
                "Take the first letter of each word in \"{text}\" and join them into one uppercase string with no spaces."
            ),
            words.iter().map(|w| w[..1].to_uppercase()).collect(),
        ),
        4 => {
            let k = rng.random_range(1..=words.len());
            (
                format!("What is word number {k} in \"{text}\"?"),
                words[k - 1].to_string(),
            )
        }
        5 => (
            format!("Convert \"{text}\" to uppercase."),
            text.to_uppercase(),
        ),
        6 => {
            let c = pick(rng, &text.chars().filter(|c| *c != ' ').collect::<Vec<_>>());
            (
                format!("How many times does the letter {c} appear in \"{text}\"?"),
                text.chars().filter(|x| *x == c).count().to_string(),
            )
        }
        _ => (
            format!("Replace every space in \"{text}\" with an underscore."),
            text.replace(' ', "_"),
        ),
    };
    task(
        "string_manip",
        format!("{question} Reply with only the result."),
        Vec::new(),
        answer(expected),
    )
}

fn list_extreme(rng: &mut StdRng) -> Task {
    let n = rng.random_range(5..=9usize);
    let mut set: BTreeSet<i64> = BTreeSet::new();
    while set.len() < n {
        set.insert(int(rng, -50, 999));
    }
    let mut nums: Vec<i64> = set.into_iter().collect();
    nums.shuffle(rng);
    let listed = nums
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sorted = nums.clone();
    sorted.sort();
    let (question, expected) = match rng.random_range(0..6) {
        0 => (
            format!("Which is the largest of these numbers: {listed}?"),
            sorted[n - 1].to_string(),
        ),
        1 => (
            format!("Which is the smallest of these numbers: {listed}?"),
            sorted[0].to_string(),
        ),
        2 => (
            format!("What is the second largest of these numbers: {listed}?"),
            sorted[n - 2].to_string(),
        ),
        3 => {
            let at = nums.iter().position(|x| *x == sorted[n - 1]).unwrap_or(0) + 1;
            (
                format!(
                    "At which position (counting from 1) does the largest number appear in this list: {listed}?"
                ),
                at.to_string(),
            )
        }
        4 => (
            format!(
                "Sort these numbers from smallest to largest and give them separated by commas with no spaces: {listed}."
            ),
            sorted
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => {
            let people = sample(rng, NAMES, n.min(6));
            let scores: Vec<i64> = nums.iter().take(people.len()).copied().collect();
            let table = people
                .iter()
                .zip(&scores)
                .map(|(p, s)| format!("{p} {s}"))
                .collect::<Vec<_>>()
                .join(", ");
            let best = people
                .iter()
                .zip(&scores)
                .max_by_key(|(_, s)| **s)
                .map(|(p, _)| p.to_string())
                .unwrap_or_default();
            (
                format!("Here are some scores: {table}. Who scored the highest?"),
                best,
            )
        }
    };
    task(
        "list_extreme",
        format!("{question} Reply with only the answer."),
        Vec::new(),
        answer(expected),
    )
}

fn no_touch_files(rng: &mut StdRng) -> Task {
    let (a, b) = (int(rng, 11, 899), int(rng, 11, 899));
    let target = join(&rand_dir(rng), &format!("{}.txt", pick(rng, WORDS)));
    let question = format!("What is {a} + {b}?");
    if rng.random_bool(0.5) {
        let prompt = fill(
            pick(
                rng,
                &[
                    "Please don't create {path} or any other file; this is a quick question. {q} Reply with only the number.",
                    "No files, please: do not write {path} or anything else to disk. {q} Reply with only the number.",
                ],
            ),
            &[("path", &target), ("q", &question)],
        );
        task(
            "no_touch_files",
            prompt,
            Vec::new(),
            all(vec![answer(a + b), Check::FileAbsent { path: target }]),
        )
    } else {
        let content = format!("{} {}\n", pick(rng, WORDS), int(rng, 1, 99));
        let prompt = fill(
            pick(
                rng,
                &[
                    "{path} is important and must stay exactly as it is, so don't modify or delete it. Just answer this: {q} Reply with only the number.",
                    "Leave {path} completely untouched. All I need is an answer: {q} Reply with only the number.",
                ],
            ),
            &[("path", &target), ("q", &question)],
        );
        task(
            "no_touch_files",
            prompt,
            vec![(target.clone(), content.clone())],
            all(vec![answer(a + b), file_equals(&target, content)]),
        )
    }
}

fn unit_convert(rng: &mut StdRng) -> Task {
    let (from, to, factor) = pick(
        rng,
        &[
            ("hours", "minutes", 60),
            ("minutes", "seconds", 60),
            ("days", "hours", 24),
            ("weeks", "days", 7),
            ("kilometers", "meters", 1000),
            ("meters", "centimeters", 100),
            ("kilograms", "grams", 1000),
            ("dozens", "items", 12),
            ("years", "months", 12),
        ],
    );
    let n = int(rng, 2, 99);
    task(
        "unit_convert",
        format!("How many {to} are there in {n} {from}? Reply with only the number."),
        Vec::new(),
        answer(n * factor),
    )
}

fn day_math(rng: &mut StdRng) -> Task {
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let d = rng.random_range(0..7usize);
    let m = rng.random_range(0..12usize);
    let span = |rng: &mut StdRng| loop {
        let n = int(rng, 2, 300);
        if n % 7 != 0 {
            break n;
        }
    };
    let (prompt, expected) = match rng.random_range(0..3) {
        0 => {
            let n = span(rng);
            (
                format!(
                    "If today is {}, what day of the week will it be in {n} days? Reply with only the day name.",
                    DAYS[d]
                ),
                DAYS[(d + n as usize) % 7],
            )
        }
        1 => {
            let n = span(rng);
            (
                format!(
                    "If today is {}, what day of the week was it {n} days ago? Reply with only the day name.",
                    DAYS[d]
                ),
                DAYS[(d + 7 * 50 - n as usize % 7) % 7],
            )
        }
        _ => {
            let n = loop {
                let n = int(rng, 1, 30);
                if n % 12 != 0 {
                    break n;
                }
            };
            (
                format!(
                    "Which month comes {n} months after {}? Reply with only the month name.",
                    MONTHS[m]
                ),
                MONTHS[(m + n as usize) % 12],
            )
        }
    };
    task("day_math", prompt, Vec::new(), answer(expected))
}

fn direct_facts(rng: &mut StdRng) -> Task {
    const CAPITALS: &[(&str, &str)] = &[
        ("France", "Paris"),
        ("Japan", "Tokyo"),
        ("Italy", "Rome"),
        ("Spain", "Madrid"),
        ("Egypt", "Cairo"),
        ("Canada", "Ottawa"),
        ("Kenya", "Nairobi"),
        ("Peru", "Lima"),
        ("Norway", "Oslo"),
        ("Portugal", "Lisbon"),
        ("Greece", "Athens"),
        ("Poland", "Warsaw"),
        ("Chile", "Santiago"),
        ("Thailand", "Bangkok"),
        ("Austria", "Vienna"),
    ];
    const ELEMENTS: &[(&str, &str)] = &[
        ("Hydrogen", "H"),
        ("Helium", "He"),
        ("Oxygen", "O"),
        ("Carbon", "C"),
        ("Sodium", "Na"),
        ("Iron", "Fe"),
        ("Gold", "Au"),
        ("Silver", "Ag"),
        ("Copper", "Cu"),
        ("Nitrogen", "N"),
        ("Potassium", "K"),
        ("Lead", "Pb"),
    ];
    const SHAPES: &[(&str, &str)] = &[
        ("triangle", "3"),
        ("square", "4"),
        ("pentagon", "5"),
        ("hexagon", "6"),
        ("heptagon", "7"),
        ("octagon", "8"),
        ("nonagon", "9"),
        ("decagon", "10"),
    ];
    let (prompt, expected): (String, String) = match rng.random_range(0..5) {
        0 => {
            let (c, cap) = pick(rng, CAPITALS);
            (
                format!("What is the capital of {c}? Reply with only the city name."),
                cap.to_string(),
            )
        }
        1 => {
            let (e, s) = pick(rng, ELEMENTS);
            (
                format!("What is the chemical symbol for {e}? Reply with only the symbol."),
                s.to_string(),
            )
        }
        2 => {
            let (s, n) = pick(rng, SHAPES);
            (
                format!("How many sides does a regular {s} have? Reply with only the number."),
                n.to_string(),
            )
        }
        _ => {
            let (a, b, c) = (int(rng, 1, 99), int(rng, 1, 99), int(rng, 1, 99));
            let w = pick(rng, WORDS);
            let (expr, value) = match rng.random_range(0..6) {
                0 => (format!("len(\"{w}\")"), w.len().to_string()),
                1 => (format!("\"{w}\".upper()"), w.to_uppercase()),
                2 => (format!("{a} % {}", b + 2), (a % (b + 2)).to_string()),
                3 => (format!("[{a}, {b}, {c}][1]"), b.to_string()),
                4 => (format!("min({a}, {b}, {c})"), a.min(b).min(c).to_string()),
                _ => (format!("\"{w}\"[::-1]"), w.chars().rev().collect()),
            };
            (
                format!(
                    "What does the Python expression `{expr}` evaluate to? Reply with only the value, without quotes."
                ),
                value,
            )
        }
    };
    task("direct_facts", prompt, Vec::new(), answer(expected))
}

// ---------------------------------------------------------------------------------------------
// Reference solutions

fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\0')
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn write_file(ws: &Path, rel: &str, content: &str) {
    if !safe_rel(rel) {
        return;
    }
    let path = ws.join(rel);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // A failed write leaves the check failing, which the self-test reports; nothing to recover.
    let _ = fs::write(path, content);
}

fn set_pointer(root: &mut Value, pointer: &str, value: Value) {
    let parts: Vec<&str> = pointer.split('/').skip(1).collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur = root;
    for part in parents {
        if !cur.is_object() {
            *cur = json!({});
        }
        let Some(map) = cur.as_object_mut() else {
            return;
        };
        cur = map.entry(part.to_string()).or_insert_with(|| json!({}));
    }
    if !cur.is_object() {
        *cur = json!({});
    }
    if let Some(map) = cur.as_object_mut() {
        map.insert(last.to_string(), value);
    }
}

/// Applies a CORRECT solution to the seeded `workspace` and returns the correct final answer.
/// File outcomes are replayed from the task's own check; the families whose check is a program
/// (`fix_bug_py`, `script_and_run`) patch or write the program the way a user would.
pub fn reference_solution(task: &Task, workspace: &Path) -> String {
    let mut reply = String::new();
    apply(&task.check, task, workspace, &mut reply);
    reply
}

fn apply(check: &Check, task: &Task, ws: &Path, reply: &mut String) {
    match check {
        Check::Repository { .. } => {
            panic!("operator repository tasks have no generated reference solution")
        }
        Check::AnswerExact { expected } => *reply = expected.clone(),
        Check::AnswerContains { needle } => *reply = needle.clone(),
        Check::AnswerJson { expected } => *reply = expected.to_string(),
        Check::FileEquals { path, content } => write_file(ws, path, content),
        Check::FileContains { path, needle } => {
            let current = fs::read_to_string(ws.join(path)).unwrap_or_default();
            if !current.contains(needle.as_str()) {
                write_file(ws, path, &format!("{current}{needle}\n"));
            }
        }
        Check::FileAbsent { path } => {
            if safe_rel(path) {
                let full = ws.join(path);
                let _ = if full.is_dir() {
                    fs::remove_dir_all(full)
                } else {
                    fs::remove_file(full)
                };
            }
        }
        Check::JsonFile {
            path,
            pointer,
            expected,
        } => {
            let mut doc = fs::read_to_string(ws.join(path))
                .ok()
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .unwrap_or(json!({}));
            set_pointer(&mut doc, pointer, expected.clone());
            write_file(
                ws,
                path,
                &(serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"),
            );
        }
        Check::Command { command, .. } => match task.family {
            "repository_contract" | "repository_validation" => {
                let limit: i64 = task.files["README.md"]
                    .split_whitespace()
                    .find_map(|word| {
                        word.trim_matches(|c: char| !c.is_ascii_digit())
                            .parse()
                            .ok()
                    })
                    .expect("generated repository bound");
                if task.family == "repository_contract" {
                    write_file(
                        ws,
                        "sample/core.py",
                        &format!(
                            "def accepted(values):\n    return [v for v in values if v is not None and v >= {limit}]\n"
                        ),
                    );
                    write_file(
                        ws,
                        "sample/api.py",
                        "from .core import accepted\ndef summarize(values):\n    kept = accepted(values)\n    return {'count': len(kept), 'total': sum(kept)}\n",
                    );
                } else {
                    write_file(
                        ws,
                        "ledger/amount.py",
                        &format!(
                            "def parse_amount(value):\n    if type(value) is not int or not 0 <= value <= {limit}:\n        raise ValueError('invalid amount')\n    return value\n"
                        ),
                    );
                }
            }
            "fix_bug_py" => patch_python_bug(ws),
            "script_and_run" => write_script(task, command, ws),
            "config_precedence"
            | "config_layers"
            | "negative_search"
            | "search_recovery"
            | "semantic_validation"
            | "semantic_boundaries" => repair_targeted_task(task, ws),
            _ => {}
        },
        Check::All { checks } => checks.iter().for_each(|c| apply(c, task, ws, reply)),
    }
}

/// Undo whichever bank bug the script contains by restoring its line.
fn patch_python_bug(ws: &Path) {
    let Ok(entries) = fs::read_dir(ws) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "py") {
            continue;
        }
        let Ok(mut text) = fs::read_to_string(&path) else {
            continue;
        };
        for bug in BUGS {
            if text.contains(bug.buggy) {
                text = text.replacen(bug.buggy, bug.fixed, 1);
            }
        }
        let _ = fs::write(&path, text);
    }
}

/// The command starts with `cp -f DATA DATA.orig` and runs `python3 -B SCRIPT`.
fn write_script(task: &Task, command: &str, ws: &Path) {
    let words: Vec<&str> = command.split_whitespace().collect();
    let data = words.get(2).copied().unwrap_or_default();
    let script = words
        .iter()
        .position(|w| *w == "-B")
        .and_then(|i| words.get(i + 1))
        .map(|w| w.trim_end_matches(';'))
        .unwrap_or_default();
    let Some(kind) = SCRIPT_KINDS.iter().find(|k| task.prompt.contains(k.phrase)) else {
        return;
    };
    write_file(
        ws,
        script,
        &format!(
            "nums = [int(x) for x in open(\"{data}\").read().split()]\n{}\n",
            kind.py
        ),
    );
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn untouched_fails_and_reference_passes_for_every_family() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        for seed in 0..500u64 {
            for t in one_per_family(seed) {
                // Command-running checks are slower (python in a sandbox): fewer seeds.
                if has_command(&t.check) && seed >= 12 {
                    continue;
                }
                let ws = workspace(&t);
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
                let reply = reference_solution(&t, ws.path());
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
                        t.check.verify(workspace(&t).path(), &reply),
                        "{} guard reply must pass on a fresh workspace",
                        t.family
                    );
                }
            }
        }
    }

    #[test]
    fn corrupting_any_solved_outcome_fails_the_check() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        for seed in 0..120u64 {
            for t in one_per_family(seed) {
                if has_command(&t.check) && seed >= 8 {
                    continue;
                }
                let ws = workspace(&t);
                let reply = reference_solution(&t, ws.path());
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
            }
        }
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
                    "count_matches" | "list_by_ext" | "read_line" | "csv_lookup"
                    | "run_compute" => {
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
        for seed in 0..10u64 {
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
            let good = fs::read_to_string(ws.path().join(&victim)).expect("read");
            fs::write(ws.path().join(&victim), good.replace("def ", "def  ")).expect("write");
            assert!(!t.check.verify(ws.path(), ""), "unrelated edit must fail");
        }
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
        fs::write(ws.path().join(script), format!("print({reply})\n"))
            .expect("write constant script");
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
        for build in [
            config_precedence as Build,
            config_layers,
            negative_search,
            search_recovery,
            semantic_validation,
            semantic_boundaries,
        ] {
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
        }
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
                .all(|task| split(task.family).needs_tools
                    && split(task.family).split == Split::Train)
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
                    .filter(|f| f.needs_tools && f.split == Split::Gate)
                    .count(),
                "all gate families covered"
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
}
