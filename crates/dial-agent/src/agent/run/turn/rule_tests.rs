use super::completion_tests::{completion_requests, connect, response};
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

#[derive(Default)]
struct Notices(Vec<String>);
impl Ui for Notices {
    fn notice(&mut self, message: &str) {
        self.0.push(message.to_owned());
    }
}

#[test]
fn non_interrupting_rule_keeps_the_reply_and_reminds_the_next_request() {
    let server = llm::fake::serve(vec![
        response("All tests pass.", None),
        // The endpoint backend probes local slots once after its first reply.
        llm::fake::json(200, &json!({"total_slots":1})),
        response("Noted.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        let rules = agent.workspace.root().join(".dial/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("claims.md"),
            "---\ncondition: tests pass\ninterruptMode: never\nscope: text\n---\nName the command that proved the tests pass.\n",
        )
        .unwrap();
        connect(agent, &server.url);
        let mut ui = Notices::default();
        let report = agent.turn("Are we done?", &mut ui).unwrap();
        assert_eq!(report.text.trim(), "All tests pass.", "the reply is kept");
        assert!(
            ui.0.iter()
                .any(|notice| notice.contains("claims.md") && notice.contains("next request")),
            "{:?}",
            ui.0
        );
        let control = agent.session.control_state();
        assert_eq!(
            control["portable_rules"]["reminders"]
                .as_object()
                .map(|reminders| reminders.len()),
            Some(1),
            "{control}"
        );
        agent.turn("Thanks", &mut NullUi).unwrap();
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2, "a deferred rule never regenerates");
    let reminded = |request: &Value| request.to_string().contains("Name the command");
    assert!(!reminded(&requests[0]));
    assert!(reminded(&requests[1]));
}
