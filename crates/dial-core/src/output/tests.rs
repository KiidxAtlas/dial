use super::*;
use std::io::Read;

fn pipe_pair() -> (File, File) {
    let mut fds = [0; 2];
    // SAFETY: fds has room for the two descriptors pipe(2) returns.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: both descriptors were just created and are owned exactly once.
    unsafe {
        (
            File::from_raw_fd(fds[0]),
            File::from(OwnedFd::from_raw_fd(fds[1])),
        )
    }
}

#[test]
fn stalled_reader_spills_to_disk_then_drains_everything_in_order() {
    let (mut reader, writer_fd) = pipe_pair();
    let pipe = Pipe::spawn(writer_fd, 1024, 64 * 1024 * 1024).unwrap();
    let writer = StdoutWriter::over(pipe.clone());
    // Far more than a pipe buffer (64 KiB on macOS/Linux) plus the memory bound: with a
    // stalled reader every enqueue must still return promptly.
    let started = Instant::now();
    let mut expected = Vec::new();
    for n in 0..5_000 {
        let value = serde_json::json!({"n": n, "pad": "x".repeat(40)});
        writer.json(&value).unwrap();
        serde_json::to_writer(&mut expected, &value).unwrap();
        expected.push(b'\n');
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "producers waited for the reader"
    );
    assert!(writer.spooling(), "stalled output was not spooled");
    assert!(pipe.lock().spilled > 0);
    assert!(
        writer.drain(Some(Duration::from_millis(50))).is_err(),
        "nothing was read yet"
    );
    // The reader resumes: everything arrives, once, in order, and the spool is released.
    let total = expected.len();
    let collector = thread::spawn(move || {
        let mut got = vec![0; total];
        reader.read_exact(&mut got).unwrap();
        got
    });
    writer.drain(Some(Duration::from_secs(30))).unwrap();
    assert_eq!(collector.join().unwrap(), expected);
    assert!(!writer.spooling());
    assert!(!writer.failed());
}

#[test]
fn closed_reader_fails_and_raises_the_interrupt() {
    // A concurrently spawned child can inherit a pipe reader between pipe() and drop().
    // Shut down a socket endpoint instead: shutdown also affects inherited duplicates, so
    // this fixture guarantees a disconnected peer under parallel process-spawning tests.
    let (reader, writer_fd) = std::os::unix::net::UnixStream::pair().unwrap();
    reader.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    let mut out = File::from(OwnedFd::from(writer_fd));
    assert_eq!(
        out.write(b"probe").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    let pipe = Pipe::spawn(out, 1024, 1024 * 1024).unwrap();
    let mut writer = StdoutWriter::over(pipe.clone());
    let interrupt = Arc::new(AtomicBool::new(false));
    writer.set_interrupt(interrupt.clone());
    writer.text(b"hello\n").unwrap();
    let result = writer.drain(Some(Duration::from_secs(10)));
    let state = pipe.lock();
    assert!(
        result.is_err(),
        "drain returned success: busy={}, memory={}, failure={:?}",
        state.busy,
        state.memory.len(),
        state.failure
    );
    drop(state);
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(interrupt.load(Ordering::SeqCst));
    assert!(writer.failed() && writer.text(b"more").is_err());
}

#[test]
fn exit_drain_timeout_reports_spooled_output_as_lost() {
    let (_reader, writer_fd) = pipe_pair();
    let pipe = Pipe::spawn(writer_fd, 1024, 64 * 1024 * 1024).unwrap();
    let writer = StdoutWriter::over(pipe);
    for n in 0..20_000 {
        writer
            .json(&serde_json::json!({"n": n, "pad": "x".repeat(40)}))
            .unwrap();
    }
    assert!(writer.spooling(), "stalled output was not spooled");
    let error = writer.drain(Some(Duration::from_millis(50))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("spooled to disk"), "{error}");
}

#[test]
fn reserved_batches_publish_in_reservation_order() {
    let (mut reader, writer_fd) = pipe_pair();
    let pipe = Pipe::spawn(writer_fd, 1024, 1024 * 1024).unwrap();
    let writer = StdoutWriter::over(pipe);
    let first = writer.reserve();
    let second = writer.reserve();
    writer
        .json_batch(second, [serde_json::json!({"n":2})])
        .unwrap();
    writer
        .json_batch(first, [serde_json::json!({"n":1})])
        .unwrap();
    writer.drain(Some(Duration::from_secs(1))).unwrap();
    let mut got = vec![0; b"{\"n\":1}\n{\"n\":2}\n".len()];
    reader.read_exact(&mut got).unwrap();
    assert_eq!(got, b"{\"n\":1}\n{\"n\":2}\n");
}
#[test]
fn spool_budget_exhaustion_is_a_failure_not_a_hang() {
    let (_reader, writer_fd) = pipe_pair();
    let pipe = Pipe::spawn(writer_fd, 16, 4096).unwrap();
    let writer = StdoutWriter::over(pipe);
    let chunk = vec![b'x'; 1024];
    let mut failed = false;
    for _ in 0..512 {
        if writer.bytes(chunk.clone()).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "an unbounded spool would fill the disk");
    assert_eq!(writer.failure().unwrap().kind(), io::ErrorKind::StorageFull);
}

#[test]
fn disconnect_is_per_connection_and_not_a_pipe_failure() {
    let (_reader, writer_fd) = pipe_pair();
    let pipe = Pipe::spawn(writer_fd, 1024, 1024 * 1024).unwrap();
    let first = StdoutWriter::over(pipe.clone());
    let second = StdoutWriter::over(pipe);
    first.cancellation().disconnect();
    assert!(first.text(b"x").is_err());
    assert!(second.text(b"y\n").is_ok());
    assert!(!second.failed());
}
