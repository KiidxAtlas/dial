//! `--external-thinking`: the model reasons in a private scratchpad the
//! user never sees. Mechanism: provider reasoning is forced off through the thinking level
//! (`ReasoningEffort::None`), `INSTRUCTION` is added to the system prompt, and `<scratchpad>`
//! blocks are removed by `strip_scratchpad` before the reply is shown or stored.

use crate::agent::Agent;

/// System-prompt addition that routes private reasoning into one `<scratchpad>` block.
pub const INSTRUCTION: &str = "Reason privately before answering. Put all scratch reasoning inside one <scratchpad>...</scratchpad> block at the start of your reply. Text outside that block is shown to the user.";

const OPEN: &str = "<scratchpad>";
const CLOSE: &str = "</scratchpad>";

/// Removes every `<scratchpad>...</scratchpad>` block. An unterminated block hides the rest of the
/// reply, because the scratchpad is private by definition.
pub fn strip_scratchpad(text: &str) -> String {
    if !text.contains(OPEN) {
        return text.to_owned();
    }
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        kept.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

impl Agent<'_> {
    /// The system-prompt addition for `--external-thinking`, or `None` when the flag is off.
    pub fn external_thinking_instruction(&self) -> Option<&'static str> {
        self.modes.launch.external_thinking.then_some(INSTRUCTION)
    }

    /// `/external-thinking on|off`: the same per-process switch as `--external-thinking`, never
    /// persisted. The scratchpad instruction lives in the system prompt, so it is rebuilt now and
    /// the next request carries the change.
    pub fn set_external_thinking(&mut self, enabled: bool) {
        if self.modes.launch.external_thinking != enabled {
            self.modes.launch.external_thinking = enabled;
            self.rebuild_system();
        }
    }

    pub fn external_thinking_enabled(&self) -> bool {
        self.modes.launch.external_thinking
    }
}

#[cfg(test)]
mod tests {
    use super::strip_scratchpad;

    #[test]
    fn strips_scratchpad_blocks_and_keeps_visible_text() {
        assert_eq!(
            strip_scratchpad("<scratchpad>secret</scratchpad>\nHello"),
            "Hello"
        );
        assert_eq!(
            strip_scratchpad("A<scratchpad>x</scratchpad>B<scratchpad>y</scratchpad>C"),
            "ABC"
        );
        assert_eq!(strip_scratchpad("plain reply"), "plain reply");
    }

    #[test]
    fn unterminated_scratchpad_hides_the_remainder() {
        assert_eq!(strip_scratchpad("shown<scratchpad>private"), "shown");
    }

    #[test]
    fn toggling_rebuilds_the_system_prompt_for_the_next_request() {
        use crate::agent::Agent;
        use crate::config::types::{Config, Version};
        use crate::llm::provider::{Backend, ProviderKind};
        let config = Config {
            model: "test".into(),
            ..Config::default()
        };
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        );
        let workspace = tempfile::tempdir().unwrap();
        let version = Version {
            id: 0,
            parent: None,
            kind: "config".into(),
            harness: config.harness.clone(),
            adapter: None,
            status: "active".into(),
        };
        let mut agent =
            Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
        let has_instruction = |agent: &Agent<'_>| {
            agent.system_messages().iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(super::INSTRUCTION.trim()))
            })
        };
        assert!(!has_instruction(&agent));
        agent.set_external_thinking(true);
        assert!(agent.external_thinking_enabled() && has_instruction(&agent));
        agent.set_external_thinking(false);
        assert!(!agent.external_thinking_enabled() && !has_instruction(&agent));
    }
}
