use super::*;
use crate::app::shell::testkit::shell;
use crate::llm::provider::{Backend, ProviderKind};
use crate::ui::transcript::TranscriptCtx;

#[test]
fn permissions_explain_native_path_scope_on_add_list_remove_and_usage() {
    for command in [
        "/permissions deny read --path '*.pem'",
        "/permissions",
        "/permissions remove 1",
        "/permissions unknown",
    ] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        shell.command(&app, &mut agent, command).unwrap();
        let ctx = TranscriptCtx {
            theme: shell.themes.current(),
            ui: &shell.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        };
        let text = shell
            .transcript
            .document(400, &ctx)
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Path rules apply only to native",
            "does not block bash cat, grep, glob",
            "Deny those tools explicitly",
            "filesystem confinement",
        ] {
            assert!(
                text.contains(expected),
                "{command}: missing {expected:?}: {text}"
            );
        }
        if command.contains("deny read") {
            assert_eq!(agent.permission_rules().len(), 1);
            assert_eq!(agent.permission_rules()[0].paths, ["*.pem"]);
            assert_eq!(agent.permission_rules()[0].tool, "read");
        }
    }
}

#[test]
fn permission_removal_picker_shows_rule_paths_and_scope_limitation() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    shell
        .command(&app, &mut agent, "/permissions deny read --path '*.pem'")
        .unwrap();
    let picker = shell.picker(&app, &agent, "permissions", "remove").unwrap();
    assert!(picker.items[0].detail.contains("--path *.pem"));
    assert!(
        picker
            .summary
            .contains("does not block bash cat, grep, glob")
    );
    assert!(picker.summary.contains("filesystem confinement"));
}

fn permission_agent(app: &App) -> Agent<'_> {
    // No request is sent: the backend only supplies the agent's endpoint identity.
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        "unused-python".into(),
        1,
    );
    Agent::interactive(
        &app.config,
        backend,
        app.layout.clone(),
        Session::ephemeral(),
        None,
        None,
        app.interrupt.clone(),
    )
    .unwrap()
}
