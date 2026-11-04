use super::{HELP_TOPICS, topic_help};
use std::collections::HashSet;
use std::path::Path;

#[test]
fn every_help_topic_resolves_to_a_document_and_section() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut names = HashSet::new();
    for &(name, destination, introduction) in HELP_TOPICS {
        assert!(names.insert(name), "duplicate help topic: {name}");
        let (file, anchor) = destination
            .split_once('#')
            .map_or((destination, None), |(file, anchor)| (file, Some(anchor)));
        let text = std::fs::read_to_string(root.join(file))
            .unwrap_or_else(|error| panic!("{destination}: {error}"));
        if let Some(anchor) = anchor {
            assert!(
                text.lines()
                    .filter_map(|line| {
                        let heading = line.strip_prefix('#')?.trim_start_matches('#').trim();
                        Some(
                            heading
                                .chars()
                                .filter_map(|ch| {
                                    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                                        Some(ch.to_ascii_lowercase())
                                    } else if ch == ' ' {
                                        Some('-')
                                    } else {
                                        None
                                    }
                                })
                                .collect::<String>(),
                        )
                    })
                    .any(|heading| heading == anchor),
                "missing section: {destination}"
            );
        }
        assert!(!introduction.is_empty());
        let help = topic_help(name).expect("mapped topic should resolve");
        assert!(help.starts_with(destination));
        assert!(help.contains(introduction));
        assert_eq!(topic_help(&name.to_ascii_uppercase()), Some(help));
    }
    // Existing command-list modes must keep going through their original dispatch.
    for topic in ["", "all", "text", "commands", "unknown-topic"] {
        assert_eq!(topic_help(topic), None);
    }
}

#[test]
fn exact_command_help_includes_shared_cli_hints_and_resolves_aliases() {
    for (command, cli) in [
        ("sessions", "sessions"),
        ("models", "models"),
        ("export", "sessions export"),
        ("commit", "commit"),
    ] {
        let help = super::command_help(command).unwrap();
        assert!(help.contains(&format!("also: dial {cli}")), "{help}");
    }
    assert_eq!(super::command_help("models"), super::command_help("model"));
    assert!(super::command_help("unknown-topic").is_none());
}
