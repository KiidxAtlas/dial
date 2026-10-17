//! Replaying a stored session into the transcript on resume.

use crate::storage::session::Session;
use crate::ui::transcript::Transcript;
use serde_json::Value;

/// Show an earlier session's conversation as transcript blocks (for `dial -c`).
pub(super) fn replay_session(transcript: &mut Transcript, session: &Session) {
    replay(transcript, &session.messages());
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

fn replay(transcript: &mut Transcript, messages: &[Value]) {
    let mut replay = crate::ui::transcript::Replay::default();
    for message in messages {
        replay.push(transcript, message);
    }
    transcript.finish_turn();
}
