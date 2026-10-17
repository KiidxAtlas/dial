use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;

struct Env {
    _home: tempfile::TempDir,
    _proj: tempfile::TempDir,
    layout: Layout,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(proj.path(), home.path().join("dial")).unwrap();
    Env {
        _home: home,
        _proj: proj,
        layout,
    }
}

fn put(root: &Path, name: &str, file: &str, text: &str) {
    let p = root.join(name).join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\nDo the thing.\n")
}

fn manage(layout: &Layout, args: Value) -> ToolOutput {
    manage_tool::call(layout, &args)
}

#[test]
fn frontmatter_subset_parses() {
    let cases: &[(&str, Option<&str>)] = &[
        (
            "---\nname: a\ndescription: Plain text\n---\nbody",
            Some("Plain text"),
        ),
        (
            "---\r\nname: a\r\ndescription: Windows line endings\r\n---\r\nbody",
            Some("Windows line endings"),
        ),
        (
            "\u{feff}---\nname: a\ndescription: With BOM\n---\n",
            Some("With BOM"),
        ),
        (
            "---\nname: a\ndescription: \"Quoted: with colon and \\\"escape\\\"\"\n---\n",
            Some("Quoted: with colon and \"escape\""),
        ),
        (
            "---\nname: a\ndescription: 'It''s single'\n---\n",
            Some("It's single"),
        ),
        (
            "---\nname: a\ndescription: >\n  folded over\n  two lines\n---\n",
            Some("folded over two lines"),
        ),
        (
            "---\nname: a\ndescription: |\n  literal\n  block\n---\n",
            Some("literal block"),
        ),
        (
            "---\nname: a\ndescription: plain with # comment\n---\n",
            Some("plain with"),
        ),
        (
            "---\nname: a\ndescription: has#hash\n---\n",
            Some("has#hash"),
        ),
        (
            "---\n# a comment\nname: a\nmetadata:\n  author: me\n  version: 2\ndescription: After nested map\n---\n",
            Some("After nested map"),
        ),
        (
            "---\nname: a\ndescription: multi\n  line plain\n---\n",
            Some("multi line plain"),
        ),
        ("---\nname: a\n---\nbody", None),
        ("---\nname: a\ndescription:\n---\n", None),
        ("---\nname: a\ndescription: never closed\n", None),
        ("name: a\ndescription: no fence\n", None),
    ];
    for (text, want) in cases {
        let got = split_frontmatter(text)
            .and_then(|(fm, _)| field(&parse_fields(fm), "description").map(|f| one_line(&f.value)))
            .filter(|d| !d.is_empty());
        assert_eq!(got.as_deref(), *want, "{text:?}");
    }
    let (fm, body) = split_frontmatter("---\nname: a\n---\nthe body\n").unwrap();
    assert_eq!((fm, body), ("name: a\n", "the body\n"));
}

#[test]
fn discovery_cache_reflects_every_change() {
    let e = env();
    let [project, _user] = e.layout.skills_dirs();
    put(&project, "alpha", SKILL_FILE, &skill_md("alpha", "first"));
    let names = |l: &Layout| -> Vec<(String, String)> {
        discover_all(l)
            .into_iter()
            .map(|s| (s.name, s.description))
            .collect()
    };
    assert_eq!(names(&e.layout), [("alpha".into(), "first".into())]);
    // Unchanged inputs are served from the cache and stay identical.
    assert_eq!(names(&e.layout), [("alpha".into(), "first".into())]);
    // In-place SKILL.md edit: same directory mtime is not trusted, file stat is.
    put(&project, "alpha", SKILL_FILE, &skill_md("alpha", "second!"));
    assert_eq!(names(&e.layout), [("alpha".into(), "second!".into())]);
    put(&project, "beta", SKILL_FILE, &skill_md("beta", "new"));
    assert_eq!(
        names(&e.layout),
        [
            ("alpha".into(), "second!".into()),
            ("beta".into(), "new".into())
        ]
    );
    fs::remove_dir_all(project.join("alpha")).unwrap();
    assert_eq!(names(&e.layout), [("beta".into(), "new".into())]);
}

#[test]
fn discovery_precedence_and_filtering() {
    let e = env();
    let [project, user] = e.layout.skills_dirs();
    put(
        &project,
        "shared",
        SKILL_FILE,
        &skill_md("shared", "from project"),
    );
    put(
        &user,
        "shared",
        SKILL_FILE,
        &skill_md("shared", "from user"),
    );
    put(
        &user,
        "only-user",
        SKILL_FILE,
        &skill_md("only-user", "user only"),
    );
    put(
        &project,
        "mismatch",
        SKILL_FILE,
        &skill_md("other", "name differs from dir"),
    );
    put(
        &project,
        "No_Good",
        SKILL_FILE,
        &skill_md("No_Good", "bad dir name"),
    );
    put(
        &project,
        "no-desc",
        SKILL_FILE,
        "---\nname: no-desc\n---\nbody",
    );
    put(&project, "no-front", SKILL_FILE, "just text");
    put(
        &project,
        "implicit-name",
        SKILL_FILE,
        "---\ndescription: name taken from directory\n---\nbody",
    );
    fs::create_dir_all(project.join("empty-dir")).unwrap();
    fs::write(project.join("stray-file.md"), "x").unwrap();

    let skills = discover(&e.layout);
    let summary: Vec<_> = skills
        .iter()
        .map(|s| (s.name.as_str(), s.description.as_str(), s.source))
        .collect();
    assert_eq!(
        summary,
        [
            ("implicit-name", "name taken from directory", Scope::Project),
            ("only-user", "user only", Scope::User),
            ("shared", "from project", Scope::Project),
        ]
    );
    assert_eq!(skills[2].path, project.join("shared").join(SKILL_FILE));
}

#[test]
#[cfg(unix)]
fn discovery_ignores_symlinked_skills_and_escaping_project_dir() {
    let e = env();
    let [project, user] = e.layout.skills_dirs();
    let outside = tempfile::tempdir().unwrap();
    put(
        outside.path(),
        "evil",
        SKILL_FILE,
        &skill_md("evil", "outside"),
    );
    fs::create_dir_all(&project).unwrap();
    symlink(outside.path().join("evil"), project.join("evil")).unwrap();
    put(&project, "linked-file", "other.md", "x");
    symlink(
        outside.path().join("evil").join(SKILL_FILE),
        project.join("linked-file").join(SKILL_FILE),
    )
    .unwrap();
    assert!(discover(&e.layout).is_empty());

    // A whole project skills dir that is a symlink out of the project is ignored too.
    fs::remove_dir_all(&project).unwrap();
    symlink(outside.path(), &project).unwrap();
    assert!(discover(&e.layout).is_empty());
    // ... while a symlinked user dir (dotfiles setups) is the user's own choice.
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    symlink(outside.path(), &user).unwrap();
    assert_eq!(discover(&e.layout).len(), 1);
}

#[test]
fn catalog_lists_and_caps() {
    assert_eq!(catalog_block(&[]), "");
    let mk = |n: usize, d: String| Skill {
        name: format!("s{n}"),
        description: d,
        path: PathBuf::from("/x/SKILL.md"),
        source: Scope::User,
        origin: "Dial",
    };
    let few = catalog_block(&[mk(1, "does one".into()), mk(2, "does two".into())]);
    assert!(
        few.contains("- s1: does one\n")
            && few.contains("- s2: does two\n")
            && !few.contains("more skills")
    );
    let long = catalog_block(&[mk(1, "x".repeat(500))]);
    assert!(long.contains('…') && long.chars().count() < 600);
    let many: Vec<_> = (0..100).map(|n| mk(n, "y".repeat(150))).collect();
    let big = catalog_block(&many);
    assert!(
        big.chars().count() <= CATALOG_BUDGET_CHARS + 120,
        "{}",
        big.chars().count()
    );
    assert!(big.contains("more skills") && big.contains("- s0:"));
}

#[test]
fn skill_tool_returns_body_without_frontmatter_and_lists_bundled_files() {
    let e = env();
    let [project, _] = e.layout.skills_dirs();
    put(
        &project,
        "deploy",
        SKILL_FILE,
        "---\nname: deploy\ndescription: Ship it\n---\n\n# Deploy\nRun scripts/ship.sh\n",
    );
    put(&project, "deploy", "scripts/ship.sh", "echo ship\n");
    put(&project, "deploy", "notes.md", "notes");

    let out = skill_tool::call(&e.layout, &json!({"name": "deploy"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("# Deploy\nRun scripts/ship.sh"));
    assert!(
        !out.text.contains("description: Ship it"),
        "frontmatter must be stripped"
    );
    assert!(
        out.text.contains("- notes.md (5 bytes)")
            && out.text.contains("- scripts/ship.sh (10 bytes)")
    );
    assert!(!out.text.contains("- SKILL.md"));

    let file = skill_tool::call(
        &e.layout,
        &json!({"name": "deploy", "file": "scripts/ship.sh"}),
    );
    assert!(
        !file.is_error && file.text.ends_with("echo ship\n"),
        "{}",
        file.text
    );

    let missing = skill_tool::call(&e.layout, &json!({"name": "nope"}));
    assert!(missing.is_error && missing.text.contains("deploy"));
    assert!(skill_tool::call(&e.layout, &json!({"name": "deploy", "file": "absent.txt"})).is_error);
    assert!(skill_tool::call(&e.layout, &json!({"name": "deploy", "file": "scripts"})).is_error);
    assert!(skill_tool::call(&e.layout, &json!({})).is_error);
}

#[test]
#[cfg(unix)]
fn skill_tool_refuses_traversal_symlinks_and_oversize() {
    let e = env();
    let [project, _] = e.layout.skills_dirs();
    put(&project, "s", SKILL_FILE, &skill_md("s", "d"));
    put(&project, "other", SKILL_FILE, &skill_md("other", "d2"));
    put(&project, "other", "private.txt", "private");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
    fs::create_dir_all(project.join("s/data")).unwrap();
    symlink(
        outside.path().join("secret.txt"),
        project.join("s/leak.txt"),
    )
    .unwrap();
    symlink(outside.path(), project.join("s/linkdir")).unwrap();
    fs::write(
        project.join("s/data/big.txt"),
        vec![b'a'; MAX_READ_BYTES + 1],
    )
    .unwrap();
    fs::write(project.join("s/data/ok.txt"), vec![b'a'; MAX_READ_BYTES]).unwrap();
    fs::write(project.join("s/data/bin.dat"), [0xff, 0xfe, 0x00]).unwrap();

    for bad in [
        "../other/private.txt",
        "/etc/passwd",
        "data/../../other/private.txt",
        "leak.txt",
        "linkdir/secret.txt",
        "",
        "./../x",
        "data//..//x",
    ] {
        let out = skill_tool::call(&e.layout, &json!({"name": "s", "file": bad}));
        assert!(out.is_error, "{bad:?} must be refused: {}", out.text);
        assert!(!out.text.contains("top secret") && !out.text.contains("private"));
    }
    let name_escape = skill_tool::call(&e.layout, &json!({"name": "../other"}));
    assert!(name_escape.is_error);
    assert!(
        skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/big.txt"}))
            .text
            .contains("larger")
    );
    assert!(!skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/ok.txt"})).is_error);
    assert!(
        skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/bin.dat"}))
            .text
            .contains("UTF-8")
    );
    // Symlinks are not even listed as bundled files.
    let body = skill_tool::call(&e.layout, &json!({"name": "s"}));
    assert!(
        !body.text.contains("leak.txt") && !body.text.contains("linkdir"),
        "{}",
        body.text
    );
}

#[test]
fn manage_create_update_delete_round_trip() {
    let e = env();
    let created = manage(
        &e.layout,
        json!({"action": "create", "name": "rust-tests", "description": "Run \"cargo test\": fast\nsecond line", "body": "# Steps\n1. cargo test --lib\n"}),
    );
    assert!(!created.is_error, "{}", created.text);
    let path = e.layout.skills_dirs()[0]
        .join("rust-tests")
        .join(SKILL_FILE);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\nname: rust-tests\ndescription: \"Run \\\"cargo test\\\": fast second line\"\n---\n\n# Steps"), "{text}");
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "no temp files left behind"
    );

    let skills = discover(&e.layout);
    assert_eq!(skills.len(), 1);
    assert_eq!(
        skills[0].description,
        "Run \"cargo test\": fast second line"
    );

    // create refuses to overwrite
    assert!(
        manage(
            &e.layout,
            json!({"action": "create", "name": "rust-tests", "description": "d", "body": "b"})
        )
        .text
        .contains("already exists")
    );

    // update body only keeps description; description only keeps body
    assert!(
        !manage(
            &e.layout,
            json!({"action": "update", "name": "rust-tests", "body": "new body"})
        )
        .is_error
    );
    assert_eq!(
        discover(&e.layout)[0].description,
        "Run \"cargo test\": fast second line"
    );
    assert!(
        !manage(
            &e.layout,
            json!({"action": "update", "name": "rust-tests", "description": "Better description"})
        )
        .is_error
    );
    let after = skill_tool::call(&e.layout, &json!({"name": "rust-tests"}));
    assert!(after.text.contains("new body") && !after.text.contains("Steps"));
    assert_eq!(discover(&e.layout)[0].description, "Better description");

    assert!(!manage(&e.layout, json!({"action": "delete", "name": "rust-tests"})).is_error);
    assert!(discover(&e.layout).is_empty());
    assert!(manage(&e.layout, json!({"action": "delete", "name": "rust-tests"})).is_error);
}

#[test]
fn manage_update_preserves_unknown_frontmatter_keys() {
    let e = env();
    let [project, _] = e.layout.skills_dirs();
    put(
        &project,
        "k",
        SKILL_FILE,
        "---\nname: k\nlicense: MIT\ndescription: >\n  old\n  folded\nmetadata:\n  author: me\n---\nold body\n",
    );
    assert!(
        !manage(
            &e.layout,
            json!({"action": "update", "name": "k", "description": "fresh"})
        )
        .is_error
    );
    let text = fs::read_to_string(project.join("k").join(SKILL_FILE)).unwrap();
    assert_eq!(
        text,
        "---\nname: k\nlicense: MIT\ndescription: \"fresh\"\nmetadata:\n  author: me\n---\n\nold body\n"
    );
    assert_eq!(discover(&e.layout)[0].description, "fresh");
}

#[test]
fn manage_scopes_and_update_requires_existing() {
    let e = env();
    let [project, user] = e.layout.skills_dirs();
    assert!(
        manage(
            &e.layout,
            json!({"action": "update", "name": "ghost", "body": "x"})
        )
        .is_error
    );
    assert!(!manage(&e.layout, json!({"action": "create", "name": "u-skill", "description": "d", "body": "b", "scope": "user"})).is_error);
    assert!(user.join("u-skill").join(SKILL_FILE).exists() && !project.join("u-skill").exists());
    // update without a scope finds the user skill
    assert!(
        !manage(
            &e.layout,
            json!({"action": "update", "name": "u-skill", "body": "b2"})
        )
        .is_error
    );
    assert!(
        fs::read_to_string(user.join("u-skill").join(SKILL_FILE))
            .unwrap()
            .contains("b2")
    );
    // update in the wrong explicit scope fails
    assert!(
        manage(
            &e.layout,
            json!({"action": "update", "name": "u-skill", "body": "b3", "scope": "project"})
        )
        .is_error
    );
    assert!(manage(&e.layout, json!({"action": "create", "name": "x", "description": "d", "body": "b", "scope": "global"})).is_error);
    assert!(manage(&e.layout, json!({"action": "update", "name": "u-skill"})).is_error);
}

#[test]
fn manage_validates_names_content_and_secrets() {
    let e = env();
    for bad in [
        "",
        "Upper",
        "-lead",
        "has space",
        "a/b",
        "..",
        "a_b",
        &"x".repeat(65),
    ] {
        let out = manage(
            &e.layout,
            json!({"action": "create", "name": bad, "description": "d", "body": "b"}),
        );
        assert!(out.is_error, "{bad:?} must be rejected");
    }
    assert!(
        !manage(
            &e.layout,
            json!({"action": "create", "name": &"x".repeat(64), "description": "d", "body": "b"})
        )
        .is_error
    );
    assert!(
        !manage(
            &e.layout,
            json!({"action": "create", "name": "0-start", "description": "d", "body": "b"})
        )
        .is_error
    );

    let base =
        |d: &str, b: &str| json!({"action": "create", "name": "t", "description": d, "body": b});
    for out in [
        manage(&e.layout, base("", "b")),
        manage(&e.layout, base("   \n ", "b")),
        manage(&e.layout, base(&"d".repeat(MAX_DESCRIPTION_CHARS + 1), "b")),
        manage(&e.layout, base("d", "")),
        manage(&e.layout, base("d", &"z".repeat(MAX_BODY_BYTES + 1))),
        manage(&e.layout, base("d", "---\nname: evil\n---\nbody")),
        manage(&e.layout, base("d", "hidden\u{202E} text")),
        manage(&e.layout, base("d", "-----BEGIN PRIVATE KEY-----\nabc")),
        manage(
            &e.layout,
            json!({"action": "create", "name": "t", "body": "b"}),
        ),
        manage(
            &e.layout,
            json!({"action": "create", "name": "t", "description": "d"}),
        ),
        manage(&e.layout, json!({"action": "explode", "name": "t"})),
    ] {
        assert!(out.is_error, "{}", out.text);
    }
    assert!(
        !e.layout.skills_dirs()[0].join("t").exists(),
        "failed creates leave nothing behind"
    );
    assert!(!manage(&e.layout, base("d", &"z".repeat(MAX_BODY_BYTES))).is_error);
}

#[test]
#[cfg(unix)]
fn manage_never_follows_symlinks() {
    let e = env();
    let [project, user] = e.layout.skills_dirs();
    let outside = tempfile::tempdir().unwrap();
    put(
        outside.path(),
        "victim",
        SKILL_FILE,
        &skill_md("victim", "outside"),
    );
    fs::write(outside.path().join("victim/keep.txt"), "keep").unwrap();

    fs::create_dir_all(&project).unwrap();
    symlink(outside.path().join("victim"), project.join("victim")).unwrap();
    let del = manage(
        &e.layout,
        json!({"action": "delete", "name": "victim", "scope": "project"}),
    );
    assert!(del.is_error);
    assert!(
        manage(
            &e.layout,
            json!({"action": "update", "name": "victim", "scope": "project", "body": "pwn"})
        )
        .is_error
    );
    assert!(manage(&e.layout, json!({"action": "create", "name": "victim", "scope": "project", "description": "d", "body": "b"})).is_error);
    assert!(outside.path().join("victim/keep.txt").exists());
    assert!(
        fs::read_to_string(outside.path().join("victim").join(SKILL_FILE))
            .unwrap()
            .contains("outside")
    );

    // A symlink inside a managed skill is removed as a link, not followed.
    assert!(!manage(&e.layout, json!({"action": "create", "name": "mine", "description": "d", "body": "b", "scope": "user"})).is_error);
    symlink(outside.path(), user.join("mine").join("link")).unwrap();
    assert!(
        !manage(
            &e.layout,
            json!({"action": "delete", "name": "mine", "scope": "user"})
        )
        .is_error
    );
    assert!(outside.path().join("victim/keep.txt").exists());

    // A project skills dir pointing out of the project is refused for writes.
    fs::remove_file(project.join("victim")).unwrap();
    fs::remove_dir(&project).unwrap();
    symlink(outside.path(), &project).unwrap();
    let out = manage(
        &e.layout,
        json!({"action": "create", "name": "n", "description": "d", "body": "b"}),
    );
    assert!(
        out.is_error && out.text.contains("outside the project"),
        "{}",
        out.text
    );
    assert!(!outside.path().join("n").exists());

    // Delete only removes directories that are skills.
    fs::remove_file(&project).unwrap();
    fs::create_dir_all(project.join("not-a-skill")).unwrap();
    fs::write(project.join("not-a-skill/data.txt"), "x").unwrap();
    assert!(
        manage(
            &e.layout,
            json!({"action": "delete", "name": "not-a-skill"})
        )
        .is_error
    );
    assert!(project.join("not-a-skill/data.txt").exists());
}

#[test]
fn schemas_accept_valid_calls() {
    for (s, args) in [
        (skill_tool::schema(), json!({"name": "a", "file": "b"})),
        (
            manage_tool::schema(),
            json!({"action": "create", "name": "a", "description": "d", "body": "b", "scope": "user"}),
        ),
    ] {
        assert!(crate::runtime::arguments::validate(&s["function"]["parameters"], &args).is_ok());
    }
    assert!(
        crate::runtime::arguments::validate(
            &skill_tool::schema()["function"]["parameters"],
            &json!({"file": "b"})
        )
        .is_err()
    );
}
