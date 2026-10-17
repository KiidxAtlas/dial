use super::*;
use serde_json::json;

#[test]
fn empty_and_help_request_usage() {
    assert_eq!(parse("").unwrap(), None);
    assert_eq!(parse("   ").unwrap(), None);
    assert_eq!(parse("help").unwrap(), None);
}

#[test]
fn plan_parses_target_lists_and_archive_flag() {
    let inv = parse("plan target=ref_diff base=main head=feat include=src include=lib exclude=target archive account=acct")
        .unwrap()
        .unwrap();
    assert_eq!(inv.op, "preflight");
    assert_eq!(inv.args["target"], "ref_diff");
    assert_eq!(inv.args["base"], "main");
    assert_eq!(inv.args["head"], "feat");
    assert_eq!(inv.args["include"], json!(["src", "lib"]));
    assert_eq!(inv.args["exclude"], json!(["target"]));
    assert_eq!(inv.args["archiveExisting"], true);
    assert_eq!(inv.args["account"], "acct");
}

#[test]
fn positionals_fill_named_parameters_in_order() {
    let inv = parse("disposition secscan_1 secf_2 false_positive")
        .unwrap()
        .unwrap();
    assert_eq!(inv.op, "disposition");
    assert_eq!(inv.args["scan"], "secscan_1");
    assert_eq!(inv.args["finding"], "secf_2");
    assert_eq!(inv.args["status"], "false_positive");

    let inv = parse("compare a b").unwrap().unwrap();
    assert_eq!(inv.args["before"], "a");
    assert_eq!(inv.args["after"], "b");
}

#[test]
fn cloud_subcommands_map_to_operations() {
    let inv = parse("cloud pull cfg_1").unwrap().unwrap();
    assert_eq!(inv.op, "cloud_pull");
    assert_eq!(inv.args["configuration"], "cfg_1");
    let inv = parse("cloud create repositoryId=r environmentId=e lookback=all")
        .unwrap()
        .unwrap();
    assert_eq!(inv.op, "cloud_create");
    assert_eq!(inv.args["repositoryId"], "r");
    assert_eq!(inv.args["lookback"], "all");
    assert_eq!(parse("cloud configs").unwrap().unwrap().op, "cloud_configs");
    assert_eq!(
        parse("cloud status cfg").unwrap().unwrap().op,
        "cloud_status"
    );
}

#[test]
fn rejects_unknown_and_malformed_input() {
    assert!(parse("bogus").is_err());
    assert!(parse("cloud").is_err());
    assert!(parse("cloud nope").is_err());
    assert!(parse("show a b").is_err());
    assert!(parse("plan =x").is_err());
}

#[test]
fn every_listed_subcommand_parses_without_positionals() {
    for sub in SUBCOMMANDS.iter().filter(|s| **s != "cloud") {
        assert!(parse(sub).is_ok(), "{sub} should parse");
    }
}
