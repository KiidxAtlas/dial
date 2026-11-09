//! Output minimizer: shrinks the progress noise of recognised commands before the model reads it.
//!
//! The filter is chosen from the command alone. Simple recognized commands qualify; compound
//! commands qualify only when every output-producing segment is `cd` or a recognized command that
//! selects the same filter (for example `cd web && npm install`).
//! Arbitrary program stdout and pipelines never select a Cargo filter. Filters are deterministic
//! line classifiers (drop, keep, or passing results that collapse in runs). Four rules hold:
//!
//! - a line that mentions an error, warning, panic, failure, traceback, exception or fatal condition
//!   is always kept ([`is_diagnostic`]);
//! - the result is accepted only if it still holds exactly as many such lines as its input, and only
//!   when it is meaningfully smaller ([`MIN_SAVED_BYTES`] and a tenth of the output);
//! - the one exception: a test run whose only diagnostic lines are passing runner summaries becomes
//!   a single `test result: ok.` line with the summed counts ([`passing_summary`]);
//! - the caller applies it only when the complete raw output is recoverable from an artifact.
//!
//! Terminal escape sequences and carriage-return redraws are stripped for every command.

use super::bash_interceptor;
use regex::Regex;
use std::sync::LazyLock;

/// Smaller savings are not worth an artifact reference and a note.
const MIN_SAVED_BYTES: usize = 256;
/// Shorter runs of passing lines stay as they are: a marker would not be shorter.
const MIN_RUN: usize = 3;

/// A minimized output and the accounting for the note that tells the model about it.
#[derive(Debug, PartialEq, Eq)]
pub struct Minimized {
    pub text: String,
    /// The filter that applied (`cargo`, `git`, `pytest`, ... or `terminal` for escape stripping alone).
    pub filter: &'static str,
    pub before: usize,
    pub after: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Keep,
    Drop,
    /// A passing-result record; three or more in a row collapse into one marker.
    Passed,
}

struct Filter {
    name: &'static str,
    classify: fn(&str) -> Class,
}

/// The minimized form of `text`, the output of `command`; `None` when nothing applies, the saving is
/// too small, or the safety check refuses the result.
pub fn minimize(command: &str, text: &str) -> Option<Minimized> {
    if text.len() < MIN_SAVED_BYTES {
        return None;
    }
    let cleaned = clean_terminal(text);
    let filter = filter_for(command);
    // A fully passing test run becomes one summary line: every diagnostic line in it was a passing
    // result count, so nothing reportable is lost. Anything else keeps the line-by-line filter.
    let summary = filter.and_then(|filter| passing_summary(&cleaned, filter));
    let summarized = summary.is_some();
    let text_out = match (summary, filter) {
        (Some(summary), _) => summary,
        (None, Some(filter)) => apply(&cleaned, filter.classify),
        (None, None) => cleaned.clone(),
    };
    if !summarized && diagnostic_lines(&text_out) != diagnostic_lines(&cleaned) {
        return None;
    }
    let saved = text.len().saturating_sub(text_out.len());
    if saved < MIN_SAVED_BYTES || saved * 10 < text.len() {
        return None;
    }
    Some(Minimized {
        filter: filter.map_or("terminal", |filter| filter.name),
        before: text.len(),
        after: text_out.len(),
        text: text_out,
    })
}

static DIAGNOSTIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)error|warn|panic|fail|traceback|exception|fatal")
        .expect("valid diagnostic regex")
});

/// Whether `line` reports (or may report) a problem. Deliberately broad: keeping too much is safe.
fn is_diagnostic(line: &str) -> bool {
    DIAGNOSTIC.is_match(line)
}

fn diagnostic_lines(text: &str) -> usize {
    text.lines().filter(|line| is_diagnostic(line)).count()
}

fn apply(text: &str, classify: fn(&str) -> Class) -> String {
    fn flush(out: &mut String, run: &mut Vec<&str>) {
        if run.len() >= MIN_RUN {
            out.push_str(&format!(
                "[{} passing lines omitted by the output filter]\n",
                run.len()
            ));
        } else {
            run.iter().for_each(|line| out.push_str(line));
        }
        run.clear();
    }
    let mut out = String::with_capacity(text.len());
    let mut run: Vec<&str> = Vec::new();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let class = if is_diagnostic(body) {
            Class::Keep
        } else {
            classify(body)
        };
        match class {
            Class::Drop => {}
            Class::Passed => run.push(line),
            Class::Keep => {
                flush(&mut out, &mut run);
                out.push_str(line);
            }
        }
    }
    flush(&mut out, &mut run);
    out
}

// ------------------------------------------------------------------------------ terminal noise

/// Removes escape sequences, then collapses carriage-return redraws to their final frame. A
/// redrawn frame that carries a diagnostic is kept as a line of its own.
fn clean_terminal(text: &str) -> String {
    let stripped = strip_escapes(text);
    let mut out = String::with_capacity(stripped.len());
    for line in stripped.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        let body = body.strip_suffix('\r').unwrap_or(body);
        if !body.contains('\r') {
            out.push_str(body);
            out.push_str(newline);
            continue;
        }
        let frames: Vec<&str> = body
            .split('\r')
            .filter(|frame| !frame.trim().is_empty())
            .collect();
        if frames.is_empty() {
            out.push_str(newline);
        }
        let last = frames.len().saturating_sub(1);
        for (index, frame) in frames.iter().enumerate() {
            if index == last {
                out.push_str(frame);
                out.push_str(newline);
            } else if is_diagnostic(frame) {
                out.push_str(frame);
                out.push('\n');
            }
        }
    }
    out
}

/// Drops CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL|ST`) and two-byte escape sequences. A sequence
/// never swallows a line break: an unterminated one ends at the newline.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                while let Some(&next) = chars.peek() {
                    match next {
                        '\u{20}'..='\u{3f}' => {
                            chars.next();
                        }
                        '\u{40}'..='\u{7e}' => {
                            chars.next();
                            break;
                        }
                        _ => break,
                    }
                }
            }
            Some(']') => {
                while let Some(&next) = chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    chars.next();
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            Some('\n') => out.push('\n'),
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------------------------- detection

fn filter_for(command: &str) -> Option<&'static Filter> {
    let Some(sole) = bash_interceptor::sole_command(command) else {
        let commands = bash_interceptor::output_commands(command)?;
        if commands.len() < 2 {
            return None;
        }
        let mut chosen: Option<&'static Filter> = None;
        for segment in commands {
            if segment.starts_with("cd ") || segment.starts_with("cd\t") {
                // `cd` can print directory names (CDPATH); it cannot emit tool progress.
                if segment.contains(['<', '>', '`', '$']) {
                    return None;
                }
                continue;
            }
            let filter = filter_for(&segment)?;
            if chosen.is_some_and(|chosen| !std::ptr::eq(chosen, filter)) {
                return None;
            }
            chosen = Some(filter);
        }
        return chosen;
    };
    if sole.contains(['<', '>', '`']) || sole.contains("$(") {
        return None;
    }
    let words = crate::runtime::arguments::split_words(&sole);
    let (program, args) = words.split_first()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match program.rsplit('/').next()? {
        "cargo" => {
            // A leading rustup `+toolchain` selector is not the subcommand.
            let args = match args.split_first() {
                Some((first, rest)) if first.starts_with('+') => rest,
                _ => &args[..],
            };
            match *args.iter().find(|arg| !arg.starts_with('-'))? {
                "build" | "b" | "check" | "c" | "clippy" | "test" | "t" | "doc" | "nextest"
                | "install" | "fetch" | "fmt" => Some(&CARGO),
                _ => None,
            }
        }
        "git" => match git_subcommand(&args)? {
            "status" => Some(&GIT_STATUS),
            "push" | "pull" | "fetch" | "clone" => Some(&GIT_TRANSPORT),
            "commit" | "checkout" | "switch" | "merge" | "rebase" | "branch" | "init" | "add"
            | "cherry-pick" | "revert" | "reset" | "stash" | "tag" => Some(&GIT_HINTS),
            _ => None,
        },
        "npm" | "pnpm" => match *args.iter().find(|arg| !arg.starts_with('-'))? {
            "install" | "i" | "ci" | "add" | "update" | "up" | "uninstall" | "remove" | "rm"
            | "install-test" | "it" => Some(&PACKAGE_INSTALL),
            _ => None,
        },
        "yarn" => match args.iter().find(|arg| !arg.starts_with('-')) {
            None | Some(&("install" | "add" | "upgrade" | "remove")) => Some(&PACKAGE_INSTALL),
            Some(_) => None,
        },
        "pytest" | "py.test" => Some(&PYTEST),
        "python" | "python3" => match args.as_slice() {
            ["-m", "pytest", ..] => Some(&PYTEST),
            ["-m", "pip", "install", ..] => Some(&PIP_INSTALL),
            _ => None,
        },
        "pip" | "pip3" => match args.first() {
            Some(&"install") => Some(&PIP_INSTALL),
            _ => None,
        },
        "uv" => match args.as_slice() {
            ["pip", "install", ..] | ["sync", ..] | ["add", ..] => Some(&PIP_INSTALL),
            _ => None,
        },
        "jest" | "vitest" => Some(&JS_TEST),
        "npx" | "bunx" => match args.iter().find(|arg| !arg.starts_with('-')) {
            Some(&("jest" | "vitest")) => Some(&JS_TEST),
            _ => None,
        },
        "go" => match *args.iter().find(|arg| !arg.starts_with('-'))? {
            "test" => Some(&GO_TEST),
            _ => None,
        },
        "apt-get" | "apt" => match *args.iter().find(|arg| !arg.starts_with('-'))? {
            "install" | "update" | "upgrade" | "dist-upgrade" | "full-upgrade" | "remove"
            | "purge" | "reinstall" | "autoremove" => Some(&APT),
            _ => None,
        },
        _ => None,
    }
}

/// The git subcommand: the first word that is not a global option or an option's value.
fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        match arg {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => {
                args.next();
            }
            _ if arg.starts_with('-') => {}
            _ => return Some(arg),
        }
    }
    None
}

// --------------------------------------------------------------------------------------- filters

static CARGO: Filter = Filter {
    name: "cargo",
    classify: classify_cargo,
};
static GIT_STATUS: Filter = Filter {
    name: "git",
    classify: classify_git_status,
};
static GIT_TRANSPORT: Filter = Filter {
    name: "git",
    classify: classify_git_transport,
};
static GIT_HINTS: Filter = Filter {
    name: "git",
    classify: classify_git_hints,
};
static PACKAGE_INSTALL: Filter = Filter {
    name: "package install",
    classify: classify_package_install,
};
static PYTEST: Filter = Filter {
    name: "pytest",
    classify: classify_pytest,
};
static GO_TEST: Filter = Filter {
    name: "go test",
    classify: classify_go_test,
};
static PIP_INSTALL: Filter = Filter {
    name: "pip install",
    classify: classify_pip_install,
};
static JS_TEST: Filter = Filter {
    name: "js test",
    classify: classify_js_test,
};
static APT: Filter = Filter {
    name: "apt",
    classify: classify_apt,
};

macro_rules! pattern {
    ($name:ident, $re:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($re).expect(concat!("valid ", stringify!($name))));
    };
}

pattern!(
    CARGO_PROGRESS,
    r"^\s*(Compiling|Checking|Fresh|Documenting|Downloaded|Downloading)\s+\S+\s+v\d|^\s*Download(ed|ing) (\d+ crates?|crates \.\.\.)|^\s*Updating (crates\.io index|git repository)"
);
pattern!(CARGO_TEST_OK, r"^test \S.* \.\.\. ok$");
pattern!(NEXTEST_PASS, r"^\s*PASS \[");

fn classify_cargo(line: &str) -> Class {
    if CARGO_PROGRESS.is_match(line) {
        Class::Drop
    } else if CARGO_TEST_OK.is_match(line) || NEXTEST_PASS.is_match(line) {
        Class::Passed
    } else {
        Class::Keep
    }
}

pattern!(
    GIT_PROGRESS,
    r"^(remote: )?(Enumerating|Counting|Compressing|Writing|Receiving|Resolving|Unpacking) (objects|deltas)|^(remote: )?Total \d+ \(delta"
);
pattern!(GIT_USAGE_HINT, r#"^\s*\(use "git [^"]*"[^)]*\)\s*$"#);

fn classify_git_hints(line: &str) -> Class {
    if line.starts_with("hint:") {
        Class::Drop
    } else {
        Class::Keep
    }
}

fn classify_git_status(line: &str) -> Class {
    if GIT_USAGE_HINT.is_match(line) {
        Class::Drop
    } else {
        classify_git_hints(line)
    }
}

fn classify_git_transport(line: &str) -> Class {
    if GIT_PROGRESS.is_match(line) {
        Class::Drop
    } else {
        classify_git_hints(line)
    }
}

pattern!(
    INSTALL_NOISE,
    r"^npm notice\b|^\d+ packages? (is|are) looking for funding|^\s*run `(npm|pnpm|yarn) fund`|^found 0 vulnerabilities|^Progress: resolved \d+|^\s*[+\-]{5,}\s*$|^\[\d/\d\] (Resolving|Fetching|Linking|Building)|^\s*[\u{2800}-\u{28ff}]\s"
);

fn classify_package_install(line: &str) -> Class {
    if INSTALL_NOISE.is_match(line) {
        Class::Drop
    } else {
        Class::Keep
    }
}

// Per-package fetch/unpack/configure progress and the package-name lists; the
// `N upgraded, N newly installed` summary and every diagnostic line stay.
pattern!(
    APT_PROGRESS,
    r"^(Get|Hit|Ign):\d+ |^Fetched |^Reading (package lists|state information)|^Building dependency tree|^\(Reading database|^Selecting previously unselected package|^Preparing to unpack|^Unpacking |^Setting up |^Processing triggers for|^update-alternatives: using|^Created symlink|^(Suggested|Recommended) packages:$|^The following (additional|NEW) packages will be installed:$|^  [a-z0-9][a-z0-9.+:~-]*( [a-z0-9][a-z0-9.+:~-]*)*\s*$|^Need to get |^After this operation|^debconf: (delaying|falling back|\(Dialog|\(No usable)|^\s*\d+%\s"
);

fn classify_apt(line: &str) -> Class {
    if APT_PROGRESS.is_match(line) {
        Class::Drop
    } else {
        Class::Keep
    }
}

pattern!(
    PYTEST_HEADER,
    r"^(platform|rootdir|plugins|cachedir|configfile|testpaths) ?[:=]?\s"
);
pattern!(
    PYTEST_PROGRESS,
    r"^(\S+\.py\s+)?\.+\s*(\[\s*\d+%\])?$|^\S+::\S.* PASSED(\s+\[\s*\d+%\])?$"
);

fn classify_pytest(line: &str) -> Class {
    if PYTEST_HEADER.is_match(line) {
        Class::Drop
    } else if PYTEST_PROGRESS.is_match(line) {
        Class::Passed
    } else {
        Class::Keep
    }
}

pattern!(GO_RUN, r"^\s*=== (RUN|PAUSE|CONT|NAME)\s");
pattern!(
    GO_PASS,
    r"^\s*--- PASS: |^ok\s+\S+\s|^\?\s+\S+\s+\[no test files\]"
);

fn classify_go_test(line: &str) -> Class {
    if GO_RUN.is_match(line) {
        Class::Drop
    } else if GO_PASS.is_match(line) {
        Class::Passed
    } else {
        Class::Keep
    }
}

pattern!(
    PIP_NOISE,
    r"^\s*(Collecting|Downloading|Using cached|Requirement already satisfied|Obtaining|Preparing metadata|Installing build dependencies|Getting requirements to build|Building wheels? for|Created wheel for|Stored in directory|Resolved \d+ packages?|Prepared \d+ packages?|Audited \d+ packages?|Downloaded \S)|^\s*[\u{2500}-\u{257f}\u{2588}]+\s+[\d.]+/[\d.]+\s"
);

fn classify_pip_install(line: &str) -> Class {
    if PIP_NOISE.is_match(line) {
        Class::Drop
    } else {
        Class::Keep
    }
}

pattern!(JS_TEST_PASS, r"^\s*(PASS|✓|√)\s+\S");

fn classify_js_test(line: &str) -> Class {
    if JS_TEST_PASS.is_match(line) {
        Class::Passed
    } else {
        Class::Keep
    }
}

// ------------------------------------------------------------------------------ passing summary

pattern!(
    CARGO_RESULT,
    r"^test result: ok\. (\d+) passed; 0 failed; (\d+) ignored\b"
);
pattern!(
    NEXTEST_RESULT,
    r"^\s*Summary \[[^\]]*\]\s+\d+ tests? run: (\d+) passed(?:, (\d+) skipped)?\s*$"
);
pattern!(
    PYTEST_RESULT,
    r"^=+ (\d+) passed(?:, (\d+) (?:skipped|deselected))? in [\d.]+s(?: \([^)]*\))? =+$"
);
pattern!(GO_RESULT, r"^ok\s+\S+\s");
pattern!(
    JEST_RESULT,
    r"^Tests:\s+(?:(\d+) skipped, )?(\d+) passed, \d+ total$"
);
pattern!(
    VITEST_RESULT,
    r"^\s*Tests\s+(\d+) passed(?: \| (\d+) skipped)? \(\d+\)$"
);

/// Passed and ignored counts from one passing runner summary line, or `None` for any other line.
fn passing_counts(filter: &Filter, line: &str) -> Option<(u64, u64)> {
    let count = |caps: &regex::Captures<'_>, i: usize| {
        caps.get(i)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0)
    };
    match filter.name {
        "cargo" => CARGO_RESULT
            .captures(line)
            .or_else(|| NEXTEST_RESULT.captures(line))
            .map(|caps| (count(&caps, 1), count(&caps, 2))),
        "pytest" => PYTEST_RESULT
            .captures(line)
            .map(|caps| (count(&caps, 1), count(&caps, 2))),
        // `go test` reports packages, not tests; each `ok` line is one passing package.
        "go test" => GO_RESULT.is_match(line).then_some((1, 0)),
        "js test" => JEST_RESULT
            .captures(line)
            .map(|caps| (count(&caps, 2), count(&caps, 1)))
            .or_else(|| {
                VITEST_RESULT
                    .captures(line)
                    .map(|caps| (count(&caps, 1), count(&caps, 2)))
            }),
        _ => None,
    }
}

/// One line for a test run that fully passed: at least one runner summary, every summary passing,
/// and no diagnostic line other than those summaries (no warning, error, panic or failure). The
/// line keeps libtest's `test result:` shape so readers parse one format. `None` otherwise.
fn passing_summary(text: &str, filter: &Filter) -> Option<String> {
    let (mut passed, mut ignored, mut suites) = (0u64, 0u64, 0u64);
    for line in text.lines() {
        match passing_counts(filter, line.trim_end()) {
            Some((p, i)) => {
                passed += p;
                ignored += i;
                suites += 1;
            }
            None if is_diagnostic(line) => return None,
            None => {}
        }
    }
    if suites == 0 {
        return None;
    }
    let unit = if filter.name == "go test" {
        "package"
    } else {
        "suite"
    };
    let plural = if suites == 1 { "" } else { "s" };
    Some(format!(
        "test result: ok. {passed} passed; 0 failed; {ignored} ignored ({suites} {unit}{plural}; passing run summarized by the output filter)\n"
    ))
}

// -------------------------------------------------------------------------------- failure digest

/// Outputs shorter than this are shown whole, so they need no digest.
const DIGEST_MIN_BYTES: usize = 4 * 1024;
/// A first error this close to the start is visible without a digest.
const DIGEST_VISIBLE_BYTES: usize = 1024;
const DIGEST_LINES: usize = 12;
const DIGEST_LINE_CHARS: usize = 200;
/// Failing tests reported with their failure detail; later ones are counted, not shown.
const DIGEST_TESTS: usize = 10;
/// Detail lines kept under each failing test's header.
const DIGEST_TEST_LINES: usize = 8;

pattern!(
    FAILURE_HEAD,
    r"^(error(\[E\d+\])?:|thread '.*' panicked at |test \S.* \.\.\. FAILED$|FAILED |E {3}|--- FAIL: |FAIL\s+\S+|npm (ERR!|error) |\S+\.\w+:\d+(:\d+)?: (fatal )?error)"
);
pattern!(LOCATION, r"^\s*--> \S+:\d+(:\d+)?\s*$");
// One failing test's report: libtest's captured-output block, pytest's section rule, Go's
// `--- FAIL:` (subtests indented) and Jest/Vitest's bullet.
pattern!(
    TEST_BLOCK,
    r"^(---- \S.* std(out|err) ----$|_{3,} \S.* _{3,}$|\s*--- FAIL: \S|\s*● \S)"
);
pattern!(
    TEST_NOISE,
    r"^(note: run with `RUST_BACKTRACE|stack backtrace:|-+ Captured )|^\s*● Console"
);
// Ends the failing-test section: libtest's name list and pytest's closing rules.
pattern!(TEST_END, r"^(failures:$|=+ )");
pattern!(
    TEST_RESULT_FAILED,
    r"^(test result: FAILED\.|=+ .*\bfailed\b.* in [\d.]+s|Tests:\s+\d+ failed|\s*Tests\s+\d+ failed|FAIL\s+\S+\s+[\d.]+s$)"
);

/// The failing tests of a test run, each header with its first detail lines (panic message and
/// location, assertion values, pytest `E` lines), then the runner's failed summary lines. `None`
/// when the output holds no recognizable failing-test report.
fn failing_test_blocks(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut summaries = Vec::new();
    let (mut tests, mut detail, mut inside) = (0usize, 0usize, false);
    for line in text.lines() {
        let trimmed = line.trim_end();
        if TEST_RESULT_FAILED.is_match(trimmed) {
            summaries.push(clip(trimmed));
            inside = false;
            continue;
        }
        if TEST_BLOCK.is_match(trimmed) && !TEST_NOISE.is_match(trimmed) {
            tests += 1;
            inside = tests <= DIGEST_TESTS;
            detail = 0;
            if inside {
                out.push(clip(trimmed));
            }
            continue;
        }
        if TEST_END.is_match(trimmed) {
            inside = false;
            continue;
        }
        if !inside || trimmed.is_empty() || TEST_NOISE.is_match(trimmed) {
            continue;
        }
        if detail < DIGEST_TEST_LINES {
            out.push(clip(trimmed));
            detail += 1;
        }
    }
    if tests == 0 {
        return None;
    }
    if tests > DIGEST_TESTS {
        out.push(format!(
            "[{} more failing tests not shown]",
            tests - DIGEST_TESTS
        ));
    }
    out.extend(summaries);
    Some(out)
}

fn clip(line: &str) -> String {
    line.chars().take(DIGEST_LINE_CHARS).collect()
}

/// The first error lines with their `file:line` locations, and the byte offset of the first.
fn error_lines(text: &str) -> Option<(Vec<String>, usize)> {
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut first = None;
    let mut after_head = false;
    for line in text.lines() {
        let start = offset;
        offset += line.len() + 1;
        let head = FAILURE_HEAD.is_match(line);
        let location = after_head && LOCATION.is_match(line);
        after_head = head;
        if !head && !location {
            continue;
        }
        first.get_or_insert(start);
        lines.push(clip(line));
        if lines.len() == DIGEST_LINES {
            break;
        }
    }
    Some((lines, first?))
}

/// For a failed build or test run of a recognized command (cargo, pytest, go test, jest, package
/// installs), a digest to show before `shown`, the text the reader sees: each failing test with
/// its failure detail, or else the first error lines with their `file:line` locations. `complete`
/// is the whole output when it is available (`shown` may be a filtered or head-and-tail view).
/// `None` for other commands, short outputs, or when everything in the digest is already
/// visible in `shown` with its first line near the top.
pub fn failure_digest(command: &str, complete: &str, shown: &str) -> Option<String> {
    let lines = digest_lines(command, complete)?;
    let visible = lines.iter().all(|line| shown.contains(line.trim()))
        && shown
            .find(lines[0].trim())
            .is_some_and(|at| at < DIGEST_VISIBLE_BYTES);
    (!visible).then(|| lines.join("\n") + "\n")
}

/// Like [`failure_digest`], for a reader that does not see the start of `text` (a capped tail
/// view of a background job), so an error near the start still needs a digest.
pub fn failure_digest_of_unseen(command: &str, text: &str) -> Option<String> {
    Some(digest_lines(command, text)?.join("\n") + "\n")
}

fn digest_lines(command: &str, text: &str) -> Option<Vec<String>> {
    if text.len() < DIGEST_MIN_BYTES {
        return None;
    }
    // A diagnostic digest can inspect a recognized build at the head of a pipeline without
    // filtering or reinterpreting the pipeline's actual output or exit status.
    if filter_for(command).is_none()
        && !bash_interceptor::pipeline_heads(command)
            .iter()
            .any(|(head, _)| filter_for(head).is_some())
    {
        return None;
    }
    let cleaned = clean_terminal(text);
    failing_test_blocks(&cleaned).or_else(|| Some(error_lines(&cleaned)?.0))
}

/// The conventional meaning of a nonzero exit `code` that printed nothing, for simple commands
/// whose status reports a result rather than a failure (grep 1 = no match).
pub fn silent_exit_meaning(command: &str, code: i32) -> Option<&'static str> {
    if code != 1 {
        return None;
    }
    let sole = bash_interceptor::sole_command(command)?;
    let words = crate::runtime::arguments::split_words(&sole);
    let (program, args) = words.split_first()?;
    match program.rsplit('/').next()? {
        "git" => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            match git_subcommand(&args)? {
                "grep" => Some("no lines matched"),
                "diff" if args.iter().any(|a| matches!(*a, "--exit-code" | "--quiet")) => {
                    Some("there are differences")
                }
                _ => None,
            }
        }
        "grep" | "egrep" | "fgrep" | "rg" | "ag" => Some("no lines matched"),
        "diff" | "cmp" => Some("the inputs differ"),
        "test" | "[" | "[[" => Some("the condition is false"),
        "pgrep" => Some("no process matched"),
        "which" => Some("the program is not on PATH"),
        _ => None,
    }
}

pattern!(
    SHELL_NOT_FOUND,
    r"(: command not found|^(\S+: )?(\d+: )?\S+: not found)$"
);

/// Failure lines that an earlier step of a compound command or pipeline printed although the
/// whole command exited zero (`step 2>&1; ls`): the reported status belongs to the last step, not
/// to the step that failed. Uses the digest's failure vocabulary plus the shell's own missing-program
/// report. A sole command's status is its own, so it gets none. At most two clipped lines; this is an
/// observation, not proof the work failed.
pub fn masked_failure(command: &str, text: &str) -> Option<String> {
    if bash_interceptor::sole_command(command).is_some() {
        return None;
    }
    let failures: Vec<String> = clean_terminal(text)
        .lines()
        .map(str::trim_end)
        .filter(|line| FAILURE_HEAD.is_match(line) || SHELL_NOT_FOUND.is_match(line.trim_start()))
        .take(2)
        .map(clip)
        .collect();
    (!failures.is_empty()).then(|| failures.join(" | "))
}

#[cfg(test)]
mod tests;
