//! Typed errors for the adapted pure-source edit algorithms.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{message}")]
    Parse { message: String, line: Option<u32> },
    #[error("{0}")]
    Apply(String),
    #[error("{0}")]
    Match(String),
    #[error("{0}")]
    NoChange(String),
}
impl EditError {
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
            line: None,
        }
    }
    pub fn apply(message: impl Into<String>) -> Self {
        Self::Apply(message.into())
    }
    pub fn matched(message: impl Into<String>) -> Self {
        Self::Match(message.into())
    }
    pub fn no_change(message: impl Into<String>) -> Self {
        Self::NoChange(message.into())
    }
}
