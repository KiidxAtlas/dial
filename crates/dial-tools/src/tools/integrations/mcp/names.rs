//! Model-facing names for remote tools: `mcp__<server>__<tool>`, restricted to what function-calling
//! APIs accept (`[A-Za-z0-9_-]`, at most 64 characters) and unique across every connected server.

use std::collections::HashSet;

const MAX_NAME: usize = 64;

/// Replaces every character outside `[A-Za-z0-9_-]` with `_`.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Hands out unique names. A name that no other tool sanitises or truncates to stays as it is; a
/// colliding one gets a suffix derived from its own server and tool name (`_` and six hex digits of
/// their SHA-256), so it does not depend on which server registered first and no remote tool
/// shadows another.
#[derive(Clone, Default)]
pub struct Namer {
    taken: HashSet<String>,
}

impl Namer {
    pub fn name(&mut self, server: &str, tool: &str) -> String {
        let base = truncate(
            &format!("mcp__{}__{}", sanitize(server), sanitize(tool)),
            MAX_NAME,
        );
        if self.taken.insert(base.clone()) {
            return base;
        }
        let digest = identity_digest(server, tool);
        let mut suffix = format!("_{digest}");
        let mut n = 2;
        loop {
            let name = format!("{}{suffix}", truncate(&base, MAX_NAME - suffix.len()));
            if self.taken.insert(name.clone()) {
                return name;
            }
            // Only the same server and tool registered twice (or a digest collision) gets here.
            suffix = format!("_{digest}_{n}");
            n += 1;
        }
    }
}

/// Six hex digits of the SHA-256 of the unsanitised server and tool names.
pub(super) fn identity_digest(server: &str, tool: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    digest[..3]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect() // the input is ASCII after sanitising, so chars are bytes
}

#[cfg(test)]
mod tests;
