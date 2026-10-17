use super::*;

struct DelayedOutput {
    pending: bool,
    data: io::Cursor<&'static [u8]>,
}

impl DelayedOutput {
    fn new() -> Self {
        Self {
            pending: true,
            data: io::Cursor::new(b"ready\n"),
        }
    }
}

impl Read for DelayedOutput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if std::mem::take(&mut self.pending) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.data.read(buffer)
    }
}

#[test]
fn terminal_reader_waits_for_nonblocking_output() {
    let ring = Arc::new(Mutex::new(Ring::terminal(1024, 24, 80)));
    spawn_reader(
        DelayedOutput::new(),
        true,
        ring.clone(),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .join()
    .unwrap();
    let ring = lock(&ring);
    assert_eq!(ring.text(), "ready\n");
    assert!(ring.closed);
    assert!(!ring.read_failed);
    assert!(ring.error.is_none());
}

/// Fails with an error that is not "no data yet".
struct Broken;

impl Read for Broken {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("pipe broke"))
    }
}

#[test]
fn pipe_reader_still_reports_unexpected_read_failures() {
    let ring = Arc::new(Mutex::new(Ring::new(1024)));
    spawn_reader(
        Broken,
        false,
        ring.clone(),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .join()
    .unwrap();
    let ring = lock(&ring);
    assert!(ring.closed);
    assert!(ring.read_failed);
    assert!(
        ring.error
            .as_ref()
            .unwrap()
            .contains("reading background output")
    );
}

/// A pipe that never reaches EOF, as when an escaped descendant keeps it open.
struct OpenPipe;

impl Read for OpenPipe {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::WouldBlock.into())
    }
}

#[test]
fn stopped_reader_closes_an_open_pipe_as_incomplete() {
    let ring = Arc::new(Mutex::new(Ring::new(1024)));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_reader(OpenPipe, false, ring.clone(), None, stop.clone()).unwrap();
    stop.store(true, Ordering::Release);
    let began = Instant::now();
    reader.join().unwrap();
    assert!(began.elapsed() < DRAIN_GRACE + Duration::from_secs(5));
    let ring = lock(&ring);
    assert!(ring.closed);
    assert!(!ring.read_failed);
    assert!(
        ring.error
            .as_deref()
            .is_some_and(|error| error.contains("incomplete"))
    );
}
