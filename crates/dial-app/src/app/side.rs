//! Terminal adapter for frontend-neutral side conversations.

pub(super) use crate::side::SideThreadState;

pub(super) fn command(
    state: &mut SideThreadState,
    args: &str,
    snapshot: Option<&crate::agent::modes::btw::SideSnapshot>,
) -> String {
    if args.trim() == "copy" {
        return match state
            .last_answer()
            .and_then(|text| super::clipboard::copy_text(&text))
        {
            Ok(text) => text.to_string(),
            Err(error) => format!("error: {error:#}"),
        };
    }
    state.command(args, snapshot)
}
