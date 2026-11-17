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

#[test]
fn terminal_size_budget_checks_combined_axes_without_allocating() {
    for (rows, cols, valid) in [
        (24, 80, true),
        (512, 512, true),
        (1, 65535, true),
        (65535, 1, true),
        (513, 512, false),
        (65535, 65535, false),
        (0, 80, false),
        (24, 0, false),
    ] {
        assert_eq!(validate_terminal_size(rows, cols).is_ok(), valid);
    }
}

#[test]
fn stopped_os_pipe_reader_finishes_while_writer_stays_open() {
    let (pipe, writer) = io::pipe().unwrap();
    let ring = Arc::new(Mutex::new(Ring::new(1024)));
    let stop = Arc::new(AtomicBool::new(true));
    let reader = spawn_pipe_reader(pipe, false, ring.clone(), None, stop).unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    let joiner = std::thread::spawn(move || {
        reader.join().unwrap();
        done.send(()).unwrap();
    });
    let result = finished.recv_timeout(Duration::from_secs(2));
    // Release the OS pipe even on regression, so a blocking reader can be reaped safely.
    drop(writer);
    joiner.join().unwrap();
    result.expect("reader must stop without waiting for the writer to close");
    let ring = lock(&ring);
    assert!(ring.closed);
    assert!(!ring.reached_eof);
    assert!(!ring.read_failed);
    assert!(ring.error.as_deref().unwrap().contains("incomplete"));
}

#[test]
fn terminal_projection_preserves_emulator_cells_and_input_modes() {
    use crate::runtime::terminal::Color;
    let mut ring = Ring::terminal(1024, 2, 8);
    ring.push(b"\x1b[?2004h\x1b[?1h\x1b[?25l\x1b[1;3;4;7;38;5;196;48;2;1;2;3m");
    ring.push("界e\u{301} ".as_bytes());
    let screen = ring.terminal_view().unwrap();
    assert!(screen.bracketed_paste && screen.application_cursor);
    assert_eq!(screen.cursor, None);
    let cell = &screen.rows[0][0];
    assert_eq!(cell.contents, "界");
    assert_eq!(cell.width, 2);
    assert_eq!(screen.rows[0][1].width, 0);
    assert_eq!(screen.rows[0][2].contents, "e\u{301}");
    assert_eq!(cell.foreground, Color::Indexed(196));
    assert_eq!(cell.background, Color::Rgb(1, 2, 3));
    assert!(cell.bold && cell.italic && cell.underline && cell.inverse);
    assert_eq!(screen.rows[0][3].background, Color::Rgb(1, 2, 3));
    ring.push(b"\x1b[?2004l\x1b[?1l\x1b[?25h");
    let screen = ring.terminal_view().unwrap();
    assert!(!screen.bracketed_paste && !screen.application_cursor);
    assert_eq!(screen.cursor, Some((0, 4)));
}
