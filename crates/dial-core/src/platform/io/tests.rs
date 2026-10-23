use super::*;

#[test]
fn queued_writer_close_drains_accepted_bytes_and_rejects_new_input() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let writer = QueuedWriter::new(file.reopen().unwrap(), 64 * 1024);
    let input = vec![b'q'; 32 * 1024];
    assert_eq!(writer.try_write(&input).unwrap(), input.len());
    writer.close();
    writer.close(); // Closing an empty or already closed queue is harmless.
    assert_eq!(
        writer.try_write(b"late").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(writer.wait_until_drained(Duration::from_secs(2)).unwrap());
    assert_eq!(writer.delivered_bytes(), input.len());
    assert!(writer.finish(Duration::from_secs(2)).unwrap());
    assert_eq!(std::fs::read(file.path()).unwrap(), input);
}
