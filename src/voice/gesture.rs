//! Press/release state machine for the voice key.
//!
//! With keyboard release reporting, holding the key records and releasing it stops (hold mode).
//! Without release reporting, each press toggles recording. A press while held-recording stops it
//! too, so a lost release cannot leave the microphone open.

/// What the controller must do after an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Start recording; `hold` means the recording ends on the next release.
    Start { hold: bool },
    /// Stop the active recording and deliver its transcript.
    Stop,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// Started by a press with release reporting; the next release stops it.
    Held,
    /// Started by a press without release reporting; the next press stops it.
    Toggled,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Gesture {
    state: State,
}

impl Gesture {
    #[cfg(test)]
    pub fn recording(&self) -> bool {
        self.state != State::Idle
    }

    /// A key press (not a repeat). `release_reporting` says whether the terminal reports releases.
    pub fn press(&mut self, release_reporting: bool) -> Command {
        match self.state {
            State::Idle => {
                self.state = if release_reporting {
                    State::Held
                } else {
                    State::Toggled
                };
                Command::Start {
                    hold: release_reporting,
                }
            }
            State::Held | State::Toggled => {
                self.state = State::Idle;
                Command::Stop
            }
        }
    }

    /// A key release. Only ends a hold-started recording.
    pub fn release(&mut self) -> Option<Command> {
        match self.state {
            State::Held => {
                self.state = State::Idle;
                Some(Command::Stop)
            }
            State::Idle | State::Toggled => None,
        }
    }

    /// Clears the state after the worker finished on its own (silence or cap).
    pub fn finished(&mut self) {
        self.state = State::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_starts_on_press_and_stops_on_release() {
        let mut g = Gesture::default();
        assert_eq!(g.press(true), Command::Start { hold: true });
        assert!(g.recording());
        assert_eq!(g.release(), Some(Command::Stop));
        assert!(!g.recording());
    }

    #[test]
    fn release_while_idle_is_ignored() {
        let mut g = Gesture::default();
        assert_eq!(g.release(), None);
        assert!(!g.recording());
    }

    #[test]
    fn toggle_fallback_ignores_release_and_stops_on_next_press() {
        let mut g = Gesture::default();
        assert_eq!(g.press(false), Command::Start { hold: false });
        assert_eq!(
            g.release(),
            None,
            "release must not stop a toggle recording"
        );
        assert!(g.recording());
        assert_eq!(g.press(false), Command::Stop);
        assert!(!g.recording());
    }

    #[test]
    fn press_while_held_stops_after_lost_release() {
        let mut g = Gesture::default();
        assert_eq!(g.press(true), Command::Start { hold: true });
        assert_eq!(
            g.press(true),
            Command::Stop,
            "a second press stops a lost-release recording"
        );
        assert!(!g.recording());
        assert_eq!(g.release(), None);
    }

    #[test]
    fn finished_worker_resets_so_next_press_starts() {
        let mut g = Gesture::default();
        assert_eq!(g.press(true), Command::Start { hold: true });
        g.finished();
        assert!(!g.recording());
        assert_eq!(g.press(true), Command::Start { hold: true });
    }
}
