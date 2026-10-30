use super::*;
use crate::platform::terminal;
use std::{
    fs::File,
    os::fd::{AsRawFd, RawFd},
    time::{Duration, Instant},
};

enum Output {
    Terminal(File),
    Pipe(io::PipeReader),
}
impl Read for Output {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Terminal(f) => f.read(bytes),
            Self::Pipe(f) => f.read(bytes),
        }
    }
}
impl AsRawFd for Output {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Terminal(f) => f.as_raw_fd(),
            Self::Pipe(f) => f.as_raw_fd(),
        }
    }
}

enum Input {
    Terminal(File),
    Pipe(io::PipeWriter),
}
impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Terminal(file) => file.write(bytes),
            Self::Pipe(pipe) => pipe.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Terminal(file) => file.flush(),
            Self::Pipe(pipe) => pipe.flush(),
        }
    }
}

pub(super) fn execute(
    command: &mut Command,
    capture: &mut Capture,
    interactive: bool,
) -> Result<i32> {
    let signals = terminal::Signals::install()?;
    let mut raw = if interactive {
        Some(terminal::RawMode::enter()?)
    } else {
        None
    };
    let mut size = crossterm::terminal::size().unwrap_or((80, 24));
    let (mut output, mut input) = if interactive {
        let (master, slave) = terminal::open(size.1, size.0)?;
        command
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        terminal::controlling_child(command);
        (
            Output::Terminal(master.try_clone()?),
            Some(Input::Terminal(master)),
        )
    } else {
        let (reader, writer) = io::pipe()?;
        crate::platform::io::set_nonblocking(&reader)?;
        command.stdout(writer.try_clone()?).stderr(writer);
        let input = if io::stdin().is_terminal() {
            // The child owns a different process group. Never let it read our foreground
            // terminal directly (SIGTTIN); relay cooked input through a nonblocking pipe.
            let (reader, writer) = io::pipe()?;
            crate::platform::io::set_nonblocking(&writer)?;
            command.stdin(reader);
            Some(Input::Pipe(writer))
        } else {
            command.stdin(Stdio::inherit());
            None
        };
        (Output::Pipe(reader), input)
    };
    let mut child = if interactive {
        process::spawn(command)
    } else {
        process::spawn_group(command)
    }
    .context("starting foreground command")?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let result = pump(
        &mut output,
        &mut input,
        &mut size,
        capture,
        &child,
        &signals,
    );
    // The leader remains unreaped, so its group id cannot be reused. Bound descendants that
    // retain output after leader exit, and clean up the owned group on I/O/storage failure.
    let _ = process::kill_group(child.id());
    // macOS can defer final PTY teardown/reaping while a master remains open.
    // The pump has already drained output (or reported failure), so close both clones first.
    drop(input);
    drop(output);
    let status = child.wait()?;
    let restoration = raw
        .as_mut()
        .map_or(Ok(()), |raw| raw.restore())
        .context("restoring foreground terminal");
    if let Err(error) = result {
        return Err(match restoration {
            Ok(()) => error,
            Err(restore_error) => error.context(format!(
                "terminal restoration also failed: {restore_error:#}"
            )),
        });
    }
    restoration?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + process::exit_signal(&status).unwrap_or(1)))
}

fn pump(
    output: &mut Output,
    input: &mut Option<Input>,
    size: &mut (u16, u16),
    capture: &mut Capture,
    child: &std::process::Child,
    signals: &terminal::Signals,
) -> Result<()> {
    let mut buffer = [0; 8192];
    let mut pending = Vec::new();
    let mut stdin_open = input.is_some();
    let mut exited = None;
    let mut eof = false;
    loop {
        let signal = signals.take();
        if signal != 0 {
            // The child is still unreaped; this group belongs to this wrapper, never a monitor.
            if let Some(identity) = crate::platform::procinfo::ProcId::of(child.id() as i32) {
                identity.signal_group(signal);
            }
        }
        if process::exited(child)? {
            let end = exited.get_or_insert_with(Instant::now);
            if eof {
                return Ok(());
            }
            ensure!(
                end.elapsed() < Duration::from_secs(2),
                "foreground leader exited but output remained open; descendant capture incomplete"
            );
            stdin_open = false;
        }
        if !stdin_open && pending.is_empty() && matches!(input, Some(Input::Pipe(_))) {
            *input = None;
        }
        if let Some(Input::Terminal(master)) = input.as_mut() {
            let next = crossterm::terminal::size().unwrap_or(*size);
            if next != *size {
                terminal::resize(master, next.1, next.0)?;
                *size = next;
            }
        }
        if let Some(sink) = input.as_mut()
            && !pending.is_empty()
        {
            match sink.write(&pending) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let (read_input, _) = terminal::ready(
            stdin_open && pending.is_empty(),
            output,
            matches!(input, Some(Input::Terminal(_))) && !pending.is_empty(),
        )?;
        if read_input {
            match io::stdin().read(&mut buffer) {
                Ok(0) => {
                    stdin_open = false;
                    if matches!(input, Some(Input::Terminal(_))) {
                        pending.push(4);
                    }
                }
                Ok(n) => pending.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !eof {
            match output.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    io::stdout().write_all(&buffer[..n])?;
                    io::stdout().flush()?;
                    capture
                        .append(&buffer[..n])
                        .context("publishing run output; capture is incomplete")?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e)
                    if matches!(output, Output::Terminal(_))
                        && e.raw_os_error() == Some(libc::EIO) =>
                {
                    eof = true
                }
                Err(e) => return Err(e.into()),
            }
        }
        if eof {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
