use super::text_tool_calls::inkling;

#[derive(Clone, Copy, Default)]
enum Channel {
    #[default]
    FrameStart,
    Text,
    Think,
    InklingText,
    InklingThinking,
    InklingCall,
}

const FRAME_HEADERS: &[(&str, Channel)] = &[
    (
        "<|message_model|><|content_thinking|>",
        Channel::InklingThinking,
    ),
    ("<|message_model|><|content_text|>", Channel::InklingText),
];

/// Streaming-safe healer for leaked reasoning tags in visible provider text.
#[derive(Default)]
pub(crate) struct Healer {
    pending: String,
    channel: Channel,
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
            let markers: &[(&str, Channel)] = match self.channel {
                Channel::FrameStart => {
                    // Only a complete header at a message boundary establishes Inkling framing.
                    let rest = self.pending.trim_start();
                    let spaces = self.pending.len() - rest.len();
                    if spaces > 0 && !visible(&self.pending[..spaces]) {
                        return false;
                    }
                    self.pending.drain(..spaces);
                    if let Some(&(tag, channel)) = FRAME_HEADERS
                        .iter()
                        .find(|(tag, _)| self.pending.starts_with(tag))
                    {
                        self.pending.drain(..tag.len());
                        self.channel = channel;
                    } else if FRAME_HEADERS
                        .iter()
                        .any(|(tag, _)| tag.starts_with(&self.pending))
                    {
                        return true;
                    } else {
                        self.channel = match inkling::header(&self.pending) {
                            inkling::Header::Incomplete => return true,
                            inkling::Header::Other => Channel::Text,
                            inkling::Header::Call { .. } => Channel::InklingCall,
                        };
                    }
                    continue;
                }
                // The tool parser owns this frame. In particular, never rewrite reasoning tags
                // inside JSON string arguments (e.g. a file's contents).
                Channel::InklingCall => &[],
                Channel::InklingText | Channel::InklingThinking => {
                    &[("<|end_message|>", Channel::FrameStart)]
                }
                Channel::Text | Channel::Think => &[
                    ("<think>", Channel::Think),
                    ("<thinking>", Channel::Think),
                    ("</think>", Channel::Text),
                    ("</thinking>", Channel::Text),
                ],
            };
            let next = markers
                .iter()
                .filter_map(|&(tag, channel)| self.pending.find(tag).map(|at| (at, tag, channel)))
                .min_by_key(|&(at, _, _)| at);
            if let Some((at, tag, channel)) = next {
                if at > 0 && !self.emit(&self.pending[..at], visible, reasoning) {
                    return false;
                }
                self.pending.drain(..at + tag.len());
                self.channel = channel;
                continue;
            }
            // Keep an entire delta when it is itself a proper marker prefix (even just `<`).
            let keep = markers
                .iter()
                .map(|(tag, _)| {
                    (1..tag.len().min(self.pending.len() + 1))
                        .rev()
                        .find(|&n| self.pending.ends_with(&tag[..n]))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                if !self.emit(&self.pending[..emit_len], visible, reasoning) {
                    return false;
                }
                self.pending.drain(..emit_len);
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
        } else if matches!(self.channel, Channel::Think | Channel::InklingThinking) {
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
