//! SSH connection-master lifetime policy over the shared runtime executor.
pub use crate::runtime::process::{Finished, Stream};
use std::{process::Command, sync::atomic::AtomicBool, time::Duration};
pub fn run(
    command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    stdout_cap: usize,
    cancel: &AtomicBool,
    on_chunk: &mut dyn FnMut(Stream, &[u8]),
) -> Result<Finished, String> {
    crate::runtime::process::run_with_policy(
        command,
        stdin,
        crate::runtime::process::Policy::ssh(timeout, stdout_cap),
        cancel,
        on_chunk,
    )
}
