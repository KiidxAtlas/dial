use super::*;

const TRACEBACK: &str = "Traceback (most recent call last):\n  File \"<dial-kernel>\", line 15, in <module>\n  File \"main.py.c\", line 13\n    int main(int argc, char **argv) {\n        ^^^^\nSyntaxError: invalid syntax\n";

fn run<'a>(tool: &'a str, source: &'a str, failed: bool, output: &'a str) -> Run<'a> {
    Run {
        tool,
        source,
        language: Some("py"),
        failed,
        output,
    }
}

fn setup() -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("polyglot")).unwrap();
    std::fs::write(
        dir.path().join("polyglot/main.py.c"),
        "#if 0\nprint(1)\n#endif\n",
    )
    .unwrap();
    (dir, vec!["polyglot/main.py.c".to_owned()])
}

#[test]
fn a_failure_stays_open_when_another_consumer_passes() {
    let (dir, changed) = setup();
    let mut runs = FailedRuns::default();
    let cell = "code = open('/app/polyglot/main.py.c').read()\nexec(code)";
    runs.observe(dir.path(), &changed, &run("eval", cell, true, TRACEBACK));
    let open = runs.unresolved(dir.path());
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1.consumer, "eval:py");
    assert!(open[0].1.summary.contains("SyntaxError"), "{:?}", open[0]);

    // Compiling the C half succeeds, but says nothing about the Python half.
    let gcc = "cd /app/polyglot && gcc main.py.c -o cmain && ./cmain 10";
    runs.observe(dir.path(), &changed, &run("bash", gcc, false, "55\n"));
    assert_eq!(runs.unresolved(dir.path()).len(), 1);

    // The same consumer passing resolves it.
    runs.observe(dir.path(), &changed, &run("eval", cell, false, "ok"));
    assert!(runs.unresolved(dir.path()).is_empty());
}

#[test]
fn editing_the_file_resolves_the_failure_and_unrelated_errors_do_not_open_one() {
    let (dir, changed) = setup();
    let mut runs = FailedRuns::default();
    runs.observe(
        dir.path(),
        &changed,
        &run("bash", "python3 polyglot/main.py.c 3", true, TRACEBACK),
    );
    assert_eq!(runs.unresolved(dir.path())[0].1.consumer, "bash:python3");
    std::fs::write(dir.path().join("polyglot/main.py.c"), "fixed").unwrap();
    assert!(runs.unresolved(dir.path()).is_empty());

    let mut runs = FailedRuns::default();
    // A missing interpreter names no location in the file.
    runs.observe(
        dir.path(),
        &changed,
        &run(
            "bash",
            "python3 main.py.c 0",
            true,
            "bash: python3: command not found\n",
        ),
    );
    // A failure in a call that never ran the file.
    runs.observe(
        dir.path(),
        &changed,
        &run("bash", "make test", true, TRACEBACK),
    );
    assert!(runs.unresolved(dir.path()).is_empty());
}

#[test]
fn compiler_locations_count_and_bare_names_do_not() {
    assert_eq!(
        diagnostic_naming("main.c:3:5: error: expected ';'\n", "main.c").as_deref(),
        Some("main.c:3:5: error: expected ';'")
    );
    assert!(diagnostic_naming("wrote main.c: done\n", "main.c").is_none());
}
