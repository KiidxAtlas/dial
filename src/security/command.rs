//! `/security` command grammar.
//!
//! Arguments are whitespace-separated tokens: positional values fill the operation's named
//! parameters in order, `key=value` sets a named parameter (repeatable for lists), and a bare
//! `archive` sets `archiveExisting`. The slash-command glue lives in `commands/security.rs`.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use super::ops::{self, Env};
use crate::config::types::Config;

pub const USAGE: &str = "Usage: /security <command> [arguments]
  plan [target=repository|scoped_path|ref_diff] [base=<rev> head=<rev>] [include=<path>]... [exclude=<path>]...
       [knowledgeBases=<path>]... [output=<dir>] [archive] [account=<client id>]
  scan [plan=<plan-id>] | status [<scan-id>] | cancel <scan-id>   run, inspect, or cancel native scans
  scans                                        list stored scans
  show <scan-id>                               scan summary and findings
  import <path>                                SARIF file or Codex Security bundle directory
  export <scan-id>                             SARIF 2.1.0 log
  validate <scan-id> <finding-id>              model-driven validation of one finding
  compare <before-scan-id> <after-scan-id>     match findings across two scans
  disposition <scan-id> <finding-id> <status>  open | false_positive | accepted_risk | fixed | wont_fix
  cloud configs [account=<client id>]
  cloud create repositoryId=<id> environmentId=<id> [repositoryUrl=<url>] [lookback=<days>|all]
  cloud status <configuration-id>
  cloud pull <configuration-id>";

/// Subcommand names for completion.
pub const SUBCOMMANDS: &[&str] = &[
    "plan",
    "scan",
    "status",
    "cancel",
    "scans",
    "cloud",
    "show",
    "import",
    "export",
    "validate",
    "compare",
    "disposition",
];

/// A parsed invocation: the operation and its JSON arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub op: &'static str,
    pub args: Value,
}

/// Runs `/security <input>` against `config` and returns the text to show.
/// Usage is returned as text; malformed input and operation failures are errors.
pub fn execute(
    config: &Config,
    input: &str,
    runner: Option<&mut dyn super::review::Reviewer>,
) -> Result<String> {
    match parse(input) {
        Ok(Some(invocation)) => ops::run(
            &Env::from_config(config),
            invocation.op,
            &invocation.args,
            runner,
        ),
        Ok(None) => Ok(USAGE.to_owned()),
        Err(error) => bail!("{error:#}\n\n{USAGE}"),
    }
}

/// Parses the command arguments. `Ok(None)` means usage was requested (no arguments or `help`).
pub fn parse(input: &str) -> Result<Option<Invocation>> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let Some((&head, rest)) = tokens.split_first() else {
        return Ok(None);
    };
    let op = match head {
        "help" | "-h" | "--help" => return Ok(None),
        "plan" => "preflight",
        "scan" => "scan",
        "status" => "status",
        "cancel" => "cancel",
        "scans" => "scans",
        "show" => "show",
        "import" => "import",
        "export" => "export",
        "validate" => "validate",
        "compare" => "compare",
        "disposition" => "disposition",
        "cloud" => {
            let Some((&sub, cloud_rest)) = rest.split_first() else {
                bail!("cloud requires one of: configs, create, status, pull");
            };
            let op = match sub {
                "configs" => "cloud_configs",
                "create" => "cloud_create",
                "status" => "cloud_status",
                "pull" => "cloud_pull",
                other => bail!("Unknown security cloud subcommand: {other}"),
            };
            return build(op, cloud_rest).map(Some);
        }
        other => bail!("Unknown security subcommand: {other}"),
    };
    build(op, rest).map(Some)
}

/// Named positional parameters for an operation, in the order they appear on the command line.
fn positional_names(op: &str) -> &'static [&'static str] {
    match op {
        "show" | "export" | "cancel" | "status" => &["scan"],
        "validate" => &["scan", "finding"],
        "import" => &["path"],
        "compare" => &["before", "after"],
        "disposition" => &["scan", "finding", "status"],
        "cloud_status" | "cloud_pull" => &["configuration"],
        _ => &[],
    }
}

/// Parameters that accept several values (`include=a include=b`).
const LIST_KEYS: &[&str] = &["include", "exclude", "knowledgeBases"];

fn build(op: &'static str, tokens: &[&str]) -> Result<Invocation> {
    let names = positional_names(op);
    let mut args = Map::new();
    let mut positionals: Vec<&str> = Vec::new();
    for &token in tokens {
        if let Some((key, value)) = token.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                bail!("Empty argument name in {token}");
            }
            if LIST_KEYS.contains(&key) {
                let entry = args
                    .entry(key.to_owned())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(items) = entry {
                    items.push(Value::String(value.to_owned()));
                }
            } else {
                args.insert(key.to_owned(), Value::String(value.to_owned()));
            }
        } else if token == "archive" {
            args.insert("archiveExisting".to_owned(), Value::Bool(true));
        } else {
            positionals.push(token);
        }
    }
    if positionals.len() > names.len() {
        bail!("Too many arguments for {op}: {}", positionals.join(" "));
    }
    for (name, value) in names.iter().zip(positionals) {
        args.entry((*name).to_owned())
            .or_insert_with(|| Value::String(value.to_owned()));
    }
    Ok(Invocation {
        op,
        args: Value::Object(args),
    })
}

#[cfg(test)]
mod tests {
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
}
