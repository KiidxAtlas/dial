//! Session, turn and execution cancellation have distinct owners and lifetimes.
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct Node {
    flag: Arc<AtomicBool>,
    parent: Option<Weak<Node>>,
    children: Mutex<Vec<Weak<Node>>>,
}
#[derive(Clone, Debug)]
pub struct Scope(Arc<Node>);
impl Scope {
    fn new(flag: Arc<AtomicBool>, parent: Option<&Self>) -> Self {
        let scope = Self(Arc::new(Node {
            flag,
            parent: parent.map(|parent| Arc::downgrade(&parent.0)),
            children: Mutex::new(Vec::new()),
        }));
        if let Some(parent) = parent {
            let mut children = parent.0.children.lock().unwrap_or_else(|e| e.into_inner());
            children.retain(|child| child.strong_count() > 0);
            children.push(Arc::downgrade(&scope.0));
        }
        scope.sync();
        scope
    }
    pub fn child(&self) -> Self {
        Self::new(Arc::new(AtomicBool::new(false)), Some(self))
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.sync();
        self.0.flag.clone()
    }
    pub fn sync(&self) {
        if self.0.flag.load(Ordering::SeqCst) {
            self.cancel();
            return;
        }
        if let Some(parent) = self.0.parent.as_ref().and_then(Weak::upgrade) {
            let parent = Self(parent);
            parent.sync();
            if parent.0.flag.load(Ordering::SeqCst) {
                self.cancel();
            }
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.sync();
        self.0.flag.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        let children: Vec<_> = self
            .0
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for child in children {
            Self(child).cancel();
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionCancellation {
    root: Scope,
}
impl Default for SessionCancellation {
    fn default() -> Self {
        Self {
            root: Scope::new(Arc::new(AtomicBool::new(false)), None),
        }
    }
}
impl SessionCancellation {
    pub fn cancel(&self) {
        self.root.cancel();
    }
    /// Attach the existing user/RPC stop flag to one turn; stopping it never cancels siblings.
    pub fn turn(&self, interrupt: Arc<AtomicBool>) -> TurnCancellation {
        TurnCancellation {
            session: self.root.clone(),
            turn: Scope::new(interrupt, Some(&self.root)),
        }
    }
}
#[derive(Clone, Debug)]
pub struct TurnCancellation {
    session: Scope,
    turn: Scope,
}
impl TurnCancellation {
    pub fn cancel(&self) {
        self.turn.cancel();
    }
    pub fn inference(&self) -> Scope {
        self.turn.child()
    }
    pub fn read(&self) -> Scope {
        self.turn.child()
    }
    pub fn background(&self) -> Scope {
        self.turn.child()
    }
    /// An admitted atomic mutation settles against session cancellation. A turn interruption is
    /// a separate host request; it must not claim an in-flight commit was rolled back.
    pub fn mutation(&self) -> MutationCancellation {
        MutationCancellation {
            commit: self.session.child(),
            interrupt: self.turn.child(),
        }
    }
    pub fn sync(&self) {
        self.turn.sync();
    }
}
#[derive(Clone, Debug)]
pub struct MutationCancellation {
    pub commit: Scope,
    pub interrupt: Scope,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interruption_and_session_termination_have_different_effect_scopes() {
        let session = SessionCancellation::default();
        let turn = session.turn(Arc::new(AtomicBool::new(false)));
        let other = session.turn(Arc::new(AtomicBool::new(false)));
        let mutation = turn.mutation();
        let read = turn.read();
        let inference = turn.inference();
        turn.cancel();
        assert!(
            mutation.interrupt.is_cancelled() && read.is_cancelled() && inference.is_cancelled()
        );
        assert!(!mutation.commit.is_cancelled() && !other.inference().is_cancelled());
        session.cancel();
        assert!(mutation.commit.is_cancelled() && other.inference().is_cancelled());
    }
    #[test]
    fn external_stop_flags_propagate_to_running_tools_on_the_next_owner_poll() {
        let session = SessionCancellation::default();
        let flag = Arc::new(AtomicBool::new(false));
        let turn = session.turn(flag.clone());
        let read = turn.read();
        let tool_flag = read.flag();
        flag.store(true, Ordering::SeqCst);
        turn.sync();
        assert!(tool_flag.load(Ordering::SeqCst));
    }
}
