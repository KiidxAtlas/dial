use std::cell::RefCell;
use std::collections::HashMap;

use super::*;

thread_local! {
    static OVERRIDES: RefCell<HashMap<String, PathBuf>> = RefCell::new(HashMap::new());
}

pub(super) fn program_override(name: &str) -> Option<OsString> {
    OVERRIDES.with(|slot| {
        slot.borrow()
            .get(name)
            .cloned()
            .map(PathBuf::into_os_string)
    })
}

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn classify_accepts_reading_statements_only() {
    for sql in [
        "select 1",
        "  WITH x AS (SELECT 1) SELECT * FROM x;",
        "(select 1)",
        "VALUES (1)",
    ] {
        assert_eq!(classify(sql).unwrap().1, Statement::Rows, "{sql}");
    }
    for sql in ["EXPLAIN SELECT 1", "show tables", "DESCRIBE t"] {
        assert_eq!(classify(sql).unwrap().1, Statement::Meta, "{sql}");
    }
    assert_eq!(classify("select 1 ;  ").unwrap().0, "select 1");
    for (sql, why) in [
        ("delete from t", "must start with"),
        ("select 1; drop table t", "one statement"),
        ("EXPLAIN ANALYZE delete from t", "ANALYZE"),
        ("explain (analyse) select 1", "ANALYZE"),
        ("select 1 -- hidden", "comments"),
        ("select 1 /* x */", "comments"),
        ("select 1\n\\! rm -rf /", "meta-commands"),
        ("select 1\n.shell ls", "meta-commands"),
        ("", "empty"),
    ] {
        let error = classify(sql).unwrap_err();
        assert!(error.contains(why), "{sql}: {error}");
    }
}

#[test]
fn urls_decode_credentials_without_form_rules() {
    let target = parse_url(
        "postgres://app%40ro:p%2Bw+d@db.example:6543/main?sslmode=require",
        &["postgres"],
    )
    .unwrap();
    assert_eq!(target.user.as_deref(), Some("app@ro"));
    assert_eq!(target.password.as_deref(), Some("p+w+d"));
    assert_eq!(target.host.as_deref(), Some("db.example"));
    assert_eq!(target.port, Some(6543));
    assert_eq!(target.database.as_deref(), Some("main"));
    assert_eq!(target.label(), "app@ro@db.example:6543/main");
    assert!(parse_url("mysql://x/y", &["postgres"]).is_err());
}

#[test]
fn postgres_plan_passes_credentials_through_env_and_forces_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let env = |name: &str| {
        (name == "DB_URL").then(|| "postgres://ro:s3cret@db:5432/app?sslmode=require".to_owned())
    };
    let args = json!({"engine":"postgres","sql":"select * from users","connection_env":"DB_URL","limit":5});
    let (plan, limit) = plan(&ws, &args, &env).unwrap();
    assert_eq!(limit, 5);
    assert_eq!(plan.program, "psql");
    assert!(!plan.args.iter().any(|arg| arg.contains("s3cret")));
    assert!(!plan.label.contains("s3cret"));
    let vars: HashMap<_, _> = plan.env.iter().cloned().collect();
    assert_eq!(vars["PGPASSWORD"], "s3cret");
    assert_eq!(vars["PGSSLMODE"], "require");
    assert!(vars["PGOPTIONS"].contains("default_transaction_read_only=on"));
    assert!(plan.stdin.contains("LIMIT 6"), "{}", plan.stdin);

    let missing = json!({"engine":"postgres","sql":"select 1","connection_env":"NOPE"});
    assert!(plan_err(&ws, &missing).contains("NOPE is not set"));
    let bad_name = json!({"engine":"postgres","sql":"select 1","connection_env":"A B"});
    assert!(plan_err(&ws, &bad_name).contains("variable name"));
}

fn plan_err(ws: &Workspace, args: &Value) -> String {
    match plan(ws, args, &no_env) {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

#[test]
fn mysql_plan_wraps_the_statement_in_a_read_only_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let env = |_: &str| Some("mysql://ro:pw@db/app".to_owned());
    let args = json!({"engine":"mysql","sql":"show tables","connection_env":"URL"});
    let (plan, _) = plan(&ws, &args, &env).unwrap();
    assert!(plan.args.contains(&"--user=ro".to_owned()));
    assert!(!plan.args.iter().any(|arg| arg.contains("pw")));
    assert!(
        plan.env
            .contains(&("MYSQL_PWD".to_owned(), "pw".to_owned()))
    );
    assert_eq!(
        plan.stdin,
        "SET SESSION TRANSACTION READ ONLY;\nSTART TRANSACTION READ ONLY;\nshow tables;\nROLLBACK;\n"
    );
}

#[test]
fn render_caps_rows_after_the_header() {
    let out = render("x", "id\n1\n2\n3\n", 2);
    assert!(out.contains("id\n1\n2\n"), "{out}");
    assert!(!out.contains("\n3\n"), "{out}");
    assert!(out.contains("more rows exist"), "{out}");
    assert!(render("x", "id\n1\n", 2).contains("[1 rows]"));
    assert!(render("x", "", 2).contains("[no output]"));
}

#[cfg(unix)]
#[test]
fn psql_receives_the_password_only_in_its_environment() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    let script = format!(
        "#!/bin/sh\n{{ printf 'argv=%s\\n' \"$*\"; printf 'pw=%s\\n' \"$PGPASSWORD\"; cat; }} > '{}'\nprintf 'n\\n1\\n'\n",
        record.display()
    );
    let bin = dir.path().join("psql");
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    OVERRIDES.with(|slot| slot.borrow_mut().insert("psql".into(), bin));
    let ws = Workspace::new(dir.path()).unwrap();
    let env = |_: &str| Some("postgres://ro:hunter2@db/app".to_owned());
    let args = json!({"engine":"postgres","sql":"select 1 as n","connection_env":"URL"});
    let out = execute(&ws, &args, &env, &AtomicBool::new(false)).unwrap();
    OVERRIDES.with(|slot| slot.borrow_mut().clear());
    assert!(out.contains("n\n1\n[1 rows]"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
    let recorded = fs::read_to_string(record).unwrap();
    assert!(recorded.contains("pw=hunter2"), "{recorded}");
    assert!(!recorded.lines().next().unwrap().contains("hunter2"));
    assert!(recorded.contains("select 1 as n"), "{recorded}");
}

/// Runs the real sqlite3 when installed: reads work, writes are refused even when hidden in a CTE.
#[test]
fn sqlite_reads_live_database_and_refuses_writes() {
    if program("sqlite3").is_err() {
        eprintln!("sqlite3 not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE t(id INTEGER, name TEXT); INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c');",
    )
    .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let run = |args: Value| execute(&ws, &args, &no_env, &AtomicBool::new(false));
    let out = run(json!({"engine":"sqlite","path":"app.db","sql":"select id, name from t order by id","limit":2})).unwrap();
    assert!(out.contains("id,name\n1,a\n2,b\n"), "{out}");
    assert!(out.contains("more rows exist"), "{out}");
    let refused = run(json!({"engine":"sqlite","path":"app.db",
        "sql":"with x as (select 1) insert into t select 9, 'z' from x"}));
    assert!(refused.is_err(), "{refused:?}");
    let count: i64 = conn
        .query_row("select count(*) from t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);
}
