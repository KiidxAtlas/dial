//! Windows foreground relay. The child leads a new console process group inside a Job Object
//! (`process::spawn_group`); stdout and stderr share one pipe read on a helper thread, so the
//! wrapper loop never blocks on output. Once the leader exits, descendants still holding the
//! pipe get the same bounded drain window as on Unix, then the job is terminated, which closes
//! every write end and ends the reader. Stdin is inherited: Windows has no background-read stop.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

pub(super) fn execute(command: &mut Command, capture: &mut Capture) -> Result<i32> {
    // A new process group ignores Ctrl+C; the first one is forwarded as Ctrl+Break, and a second
    // exits the wrapper, whose closing job handle then kills the whole tree.
    let interrupt = crate::platform::interrupt::install_sigint();
    let (mut reader, writer) = io::pipe()?;
    command
        .stdin(Stdio::inherit())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let spawned = process::spawn_group(command).context("starting foreground command");
    // Release the wrapper's write ends so end of output means every job process closed its own.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawned?;
    // At most eight chunks (128 KiB) can queue; a fast producer applies backpressure.
    let (sender, chunks) = std::sync::mpsc::sync_channel::<io::Result<Vec<u8>>>(8);
    let relay = std::thread::Builder::new()
        .name("dial-run-relay".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let chunk = match reader.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(n) => Ok(buffer[..n].to_vec()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => Err(error),
                };
                let failed = chunk.is_err();
                if sender.send(chunk).is_err() || failed {
                    return;
                }
            }
        });
    let relay = match relay {
        Ok(relay) => relay,
        Err(error) => {
            let _ = process::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            process::forget_group(child.id());
            return Err(error).context("starting the output relay");
        }
    };
    let result = pump(&chunks, capture, &child, &interrupt);
    // Bound descendants that kept output open, and clean up on relay or capture failure.
    let _ = process::kill_group(child.id());
    drop(chunks); // release a relay blocked on channel backpressure
    let status = child.wait();
    process::forget_group(child.id());
    // Once the job is dead all pipe writers close, so the reader can finish.
    let _ = relay.join();
    result?;
    let status = status?;
    // Exit codes are unsigned on Windows; report NTSTATUS-style codes bit-for-bit.
    Ok(status.code().unwrap_or(1))
}

fn pump(
    chunks: &Receiver<io::Result<Vec<u8>>>,
    capture: &mut Capture,
    child: &std::process::Child,
    interrupt: &AtomicBool,
) -> Result<()> {
    let mut exited: Option<Instant> = None;
    let mut forwarded = false;
    let mut eof = false;
    loop {
        if !forwarded && interrupt.load(Ordering::SeqCst) {
            forwarded = true;
            let _ = process::terminate_group(child.id());
        }
        if eof {
            std::thread::sleep(POLL);
        } else {
            match chunks.recv_timeout(POLL) {
                Ok(Ok(bytes)) => {
                    io::stdout().write_all(&bytes)?;
                    io::stdout().flush()?;
                    capture
                        .append_buffered(&bytes)
                        .context("publishing run output; capture is incomplete")?;
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => eof = true,
            }
        }
        capture
            .publish_due()
            .context("publishing run output; capture is incomplete")?;
        if process::exited(child)? {
            if eof {
                return Ok(());
            }
            let end = exited.get_or_insert_with(Instant::now);
            ensure!(
                end.elapsed() < DRAIN_AFTER_EXIT,
                "foreground leader exited but output remained open; descendant capture incomplete"
            );
        }
    }
}
