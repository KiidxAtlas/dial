//! Access policy shared by the forge tools (`github`, `gitea_pr`, `glab_mr`). Each tool keeps its
//! own read-only operation allowlist; approval, concurrency and resource answers follow from that
//! allowlist, so the three tools cannot drift apart. Changes to the checkout are not shared: each
//! tool declares its own `may_modify_workspace`, since only checkout ops touch the worktree.

use serde_json::Value;

use crate::tools::integrations::ext::Concurrency;
use crate::tools::surface::resources::Access;

/// The read-only operations of one forge tool. A read needs no approval, runs shared, and touches
/// no source files. Every other call, including one with a missing or unknown `op`, needs approval
/// and runs exclusive.
#[derive(Clone, Copy)]
pub(super) struct OpPolicy {
    read_ops: &'static [&'static str],
}

impl OpPolicy {
    pub(super) const fn new(read_ops: &'static [&'static str]) -> Self {
        Self { read_ops }
    }

    /// True when the call's `op` is on this tool's read-only allowlist.
    pub(super) fn is_read(self, args: &Value) -> bool {
        op_of(args).is_some_and(|op| self.read_ops.contains(&op))
    }

    pub(super) fn needs_approval(self, args: &Value) -> bool {
        !self.is_read(args)
    }

    pub(super) fn concurrency(self, args: &Value) -> Concurrency {
        if self.is_read(args) {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }

    /// Read operations query the forge and the checkout's Git metadata, never source files.
    pub(super) fn resources(self, args: &Value) -> Option<Access> {
        self.is_read(args).then(Default::default)
    }
}

/// The `op` argument when it is a string.
pub(super) fn op_of(args: &Value) -> Option<&str> {
    args["op"].as_str()
}
