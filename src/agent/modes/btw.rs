//! `/btw <question>`: a side question answered by the current model with the session as context.
//! No tools are offered and nothing is appended to the transcript; the side request uses its own cache namespace.

use crate::agent::{Agent, NullUi, make_backend};
use anyhow::{Result, ensure};
use serde_json::json;

impl Agent<'_> {
    pub fn side_question(&mut self, question: &str) -> Result<String> {
        let question = question.trim();
        ensure!(!question.is_empty(), "usage: /btw <question>");
        let config = self.effective_config().clone();
        let mut main = match self.backend.take() {
            Some(backend) => backend,
            None => make_backend(&config)?,
        };
        let result = (|| {
            let history = self.session.messages();
            let mut request = self.projected_request(&history, None)?;
            request.tools.clear();
            request.messages.push(json!({
                "role": "user",
                "content": format!(
                    "Side question (answer only this; do not call tools or continue the task): {question}"
                ),
            }));
            let mut ui = NullUi;
            self.role_chat(&mut main, "btw", &request, &mut ui)
        })();
        self.backend = Some(main);
        result
    }
}
