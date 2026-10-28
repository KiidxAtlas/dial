//! Replaying a stored session into the transcript on resume.

use crate::app::shell::plugin_effects::{PLUGIN_ENTRY_TYPE, entry_note};
use crate::storage::session::Session;
use crate::ui::transcript::Transcript;
use serde_json::Value;

/// Show an earlier session's conversation as transcript blocks (for `dial -c`).
pub(super) fn replay_session(transcript: &mut Transcript, session: &Session) {
    let records = session.custom_records(PLUGIN_ENTRY_TYPE);
    replay(transcript, &session.timestamped_messages(), &records);
    for body in session.runtime().bodies("tool") {
        if let Some(id) = body.read(|node| node.call_id.clone()) {
            transcript.bind_tool(&id, body);
        }
    }
    let streams = session.interrupted_streams();
    if !streams.is_empty() {
        transcript.notice(
            "Interrupted response",
            "Recovered output from a response that did not complete.",
        );
    }
    for body in streams {
        transcript.restore_prose(&body);
    }
}

fn replay(transcript: &mut Transcript, messages: &[Value], records: &[(usize, Value)]) {
    let mut replay = crate::ui::transcript::Replay::default();
    let mut records = records.iter().peekable();
    for (index, message) in messages.iter().enumerate() {
        while let Some((_, data)) = records.next_if(|(at, _)| *at <= index) {
            show_record(transcript, data);
        }
        replay.push(transcript, message);
    }
    for (_, data) in records {
        show_record(transcript, data);
    }
    transcript.finish_turn();
}

/// Plugin transcript records replay through the same renderer as the live entry. They are shown
/// only; model context never reads them.
fn show_record(transcript: &mut Transcript, data: &Value) {
    if let Some((title, body)) = entry_note(data) {
        transcript.notice(&title, &body);
    }
}
