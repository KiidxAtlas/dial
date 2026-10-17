//! `/fresh`: reset the provider stream chain without touching the transcript.
//! The next request is sent unchained; local session history is unchanged.

use super::super::Agent;

impl Agent<'_> {
    /// Returns false when no backend has been built yet (nothing to reset).
    pub fn reset_provider_stream(&mut self) -> bool {
        match self.backend.as_mut() {
            Some(backend) => {
                backend.reset_provider_stream();
                true
            }
            None => false,
        }
    }
}
