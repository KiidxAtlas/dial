use super::*;
use crate::args::{Cli, Command};
use clap::Parser;

#[test]
fn doctor_accepts_json_with_and_without_all_and_conflicts_with_fix() {
    let parsed = Cli::try_parse_from(["dial", "doctor", "--all", "--json"]).unwrap();
    assert!(matches!(
        parsed.command,
        Some(Command::Doctor {
            fix: None,
            yes: false,
            all: true,
            json: true,
        })
    ));
    assert!(Cli::try_parse_from(["dial", "doctor", "--fix", "bun", "--json"]).is_err());
}

#[test]
fn doctor_json_emits_typed_capabilities_and_notes() {
    let dir = tempfile::tempdir().unwrap();
    let env = dial::setup::deps::Env::current(dir.path());
    let full = doctor_json(&env, true, "Memory: local\n");
    let caps = full["capabilities"].as_array().unwrap();
    assert!(!caps.is_empty());
    for cap in caps {
        assert!(cap["id"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(cap["name"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(cap["installed"].is_boolean());
        if cap["installed"] == true {
            assert!(cap["path"].as_str().is_some());
        } else {
            assert!(cap["path"].is_null());
        }
    }
    let notes = full["notes"].as_array().unwrap();
    assert!(notes.iter().any(|n| n == "Memory: local"), "{notes:?}");
}
