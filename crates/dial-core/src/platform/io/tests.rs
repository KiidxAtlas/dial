use super::*;
use std::{
    fs::File,
    io::Write,
    os::windows::io::{AsRawHandle, FromRawHandle},
    process::{Command, Stdio},
};
use windows_sys::Win32::{
    Foundation::{DuplicateHandle, ERROR_ACCESS_DENIED, HANDLE},
    Storage::FileSystem::{FILE_WRITE_DATA, SYNCHRONIZE},
    System::{
        Pipes::{CreatePipe, GetNamedPipeInfo},
        Threading::GetCurrentProcess,
    },
};

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

// Regressions for `/COMPUTER` setup on Windows: the pipe it gets back from `ChildStdin` (or any
// write-only duplicate) cannot be queried with `GetNamedPipeInfo`, which needs
// FILE_READ_ATTRIBUTES, so validation must not depend on that right.

/// Creates an anonymous pipe as `(read end, write end)`. Both handles are non-inheritable and
/// owned by `File`s, so they close on drop.
fn anonymous_pipe() -> (File, File) {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both out pointers are valid for one HANDLE write; null attributes make the ends
    // non-inheritable and the size is a default buffer request.
    let created = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    assert_ne!(
        created,
        0,
        "CreatePipe failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: CreatePipe succeeded, so both handles are valid, unique and owned only here.
    unsafe {
        (
            File::from_raw_handle(read.cast()),
            File::from_raw_handle(write.cast()),
        )
    }
}

/// Duplicates `source` with `FILE_WRITE_DATA | SYNCHRONIZE` only, so the copy lacks
/// `FILE_READ_ATTRIBUTES` like a write-only `ChildStdin`.
fn write_only_duplicate(source: &File) -> File {
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: source lends a live handle for the call; the current-process pseudo handle is a
    // valid source and target; `duplicate` is a valid out pointer; no inheritance is requested.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source.as_raw_handle() as HANDLE,
            GetCurrentProcess(),
            &mut duplicate,
            FILE_WRITE_DATA | SYNCHRONIZE,
            0,
            0,
        )
    };
    assert_ne!(
        duplicated,
        0,
        "DuplicateHandle failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: DuplicateHandle succeeded, so `duplicate` is a new handle owned only by this File.
    unsafe { File::from_raw_handle(duplicate.cast()) }
}

/// Whether `GetNamedPipeInfo` is refused on `handle`, the premise the write-only tests rely on.
fn named_pipe_info_is_denied(handle: &File) -> bool {
    let (mut flags, mut out_size, mut in_size, mut instances) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: handle lends a live pipe for the call; the four out pointers are valid u32 slots;
    // no name buffer is requested.
    let queried = unsafe {
        GetNamedPipeInfo(
            handle.as_raw_handle() as HANDLE,
            &mut flags,
            &mut out_size,
            &mut in_size,
            &mut instances,
        )
    };
    queried == 0 && io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

#[test]
fn write_only_pipe_end_validates_in_both_modes_without_read_attributes() {
    let (read, write) = anonymous_pipe();
    let write_only = write_only_duplicate(&write);
    drop(write);
    assert!(
        named_pipe_info_is_denied(&write_only),
        "premise: a write-only pipe handle must refuse GetNamedPipeInfo"
    );
    set_nonblocking(&write_only).expect("write-only pipe end must validate for nonblocking use");
    set_blocking(&write_only).expect("write-only pipe end must validate for blocking use");
    set_nonblocking(&read).expect("read end must validate for nonblocking use");
    set_blocking(&read).expect("read end must validate for blocking use");
}

#[test]
fn write_only_payload_reaches_bounded_reader_then_eof() {
    let (mut read, write) = anonymous_pipe();
    let mut write_only = write_only_duplicate(&write);
    drop(write);
    set_nonblocking(&read).unwrap();
    let mut buffer = [0u8; 64];
    assert_eq!(
        read_nonblocking(&mut read, &mut buffer).unwrap_err().kind(),
        io::ErrorKind::WouldBlock,
        "an empty live pipe is not EOF"
    );
    let payload = b"computer-pipe-ok";
    // The synchronous write returns only after the bytes are in the pipe, so one bounded read
    // sees them all.
    write_only.write_all(payload).unwrap();
    let received = read_nonblocking(&mut read, &mut buffer).unwrap();
    assert_eq!(&buffer[..received], payload);
    drop(write_only);
    assert_eq!(
        read_nonblocking(&mut read, &mut buffer).unwrap(),
        0,
        "closing the last write end is EOF"
    );
}

#[test]
fn child_stdin_pipe_validates_in_both_modes() {
    let mut child = Command::new("cmd.exe")
        .args(["/C", "exit 0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd.exe for a real ChildStdin handle");
    let stdin = child.stdin.as_ref().expect("piped stdin is present");
    set_nonblocking(stdin).expect("ChildStdin must validate for nonblocking use");
    set_blocking(stdin).expect("ChildStdin must validate for blocking use");
    child.wait().expect("cmd.exe exits");
}

#[test]
fn ordinary_file_is_rejected_as_a_pipe_in_both_modes() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let handle = file.reopen().unwrap();
    assert_eq!(
        set_nonblocking(&handle).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        set_blocking(&handle).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}
