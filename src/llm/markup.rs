/// Streaming-safe healer for leaked reasoning tags in visible provider text.
#[derive(Default)]
pub(crate) struct Healer {
    pending: String,
    reasoning: bool,
}

impl Healer {
    pub(crate) fn push(
        &mut self,
        chunk: &str,
        visible: &mut impl FnMut(&str) -> bool,
        reasoning: &mut impl FnMut(&str) -> bool,
    ) -> bool {
        self.pending.push_str(chunk);
        loop {
            let next = ["<think>", "<thinking>", "</think>", "</thinking>"]
                .iter()
                .filter_map(|tag| self.pending.find(tag).map(|at| (at, *tag)))
                .min_by_key(|(at, _)| *at);
            if let Some((at, tag)) = next {
                if at > 0 && !self.emit(&self.pending[..at], visible, reasoning) {
                    return false;
                }
                self.pending.drain(..at + tag.len());
                self.reasoning = !tag.starts_with("</");
                continue;
            }
            let keep = (1..self.pending.len())
                .rev()
                .find(|&len| {
                    let start = self.pending.len() - len;
                    self.pending.is_char_boundary(start)
                        && ["<think>", "<thinking>", "</think>", "</thinking>"]
                            .iter()
                            .any(|tag| tag.starts_with(&self.pending[start..]))
                })
                .unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                let text = self.pending[..emit_len].to_owned();
                self.pending.drain(..emit_len);
                if !self.emit(&text, visible, reasoning) {
                    return false;
                }
            }
            return true;
        }
    }

    pub(crate) fn finish(
        &mut self,
        visible: &mut impl FnMut(&str) -> bool,
        reasoning: &mut impl FnMut(&str) -> bool,
    ) -> bool {
        let pending = std::mem::take(&mut self.pending);
        self.emit(&pending, visible, reasoning)
    }

    fn emit(
        &self,
        text: &str,
        visible: &mut impl FnMut(&str) -> bool,
        reasoning: &mut impl FnMut(&str) -> bool,
    ) -> bool {
        if text.is_empty() {
            true
        } else if self.reasoning {
            reasoning(text)
        } else {
            visible(text)
        }
    }
}

/// Remove leaked control markup from an already accumulated reply while retaining routed reasoning.
pub(crate) fn rewrite(text: &str) -> (String, String) {
    let mut healer = Healer::default();
    let mut visible = String::new();
    let mut reasoning = String::new();
    let mut push_visible = |part: &str| {
        visible.push_str(part);
        true
    };
    let mut push_reasoning = |part: &str| {
        reasoning.push_str(part);
        true
    };
    let _ = healer.push(text, &mut push_visible, &mut push_reasoning);
    let _ = healer.finish(&mut push_visible, &mut push_reasoning);
    (visible, reasoning)
}
