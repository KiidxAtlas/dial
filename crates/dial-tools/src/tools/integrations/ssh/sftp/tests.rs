use super::*;
use std::io::Cursor;

fn packet(kind: u8, id: u32, body: &[u8]) -> Vec<u8> {
    let mut data = vec![kind];
    data.extend_from_slice(&id.to_be_bytes());
    data.extend_from_slice(body);
    let mut framed = (data.len() as u32).to_be_bytes().to_vec();
    framed.extend(data);
    framed
}

fn server_prefix() -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 5, 2, 0, 0, 0, 3];
    let mut handle = Vec::new();
    string(&mut handle, b"opaque-handle");
    bytes.extend(packet(102, 1, &handle));
    bytes
}

#[test]
fn read_round_trip_uses_literal_path_and_detects_eof() {
    let mut input = server_prefix();
    let mut data = Vec::new();
    string(&mut data, b"hello\0world");
    input.extend(packet(103, 2, &data));
    input.extend(packet(101, 3, &1u32.to_be_bytes()));
    input.extend(packet(101, 4, &0u32.to_be_bytes()));
    let mut client = Client {
        reader: Cursor::new(input),
        writer: Vec::new(),
        id: 0,
    };
    assert_eq!(
        client.transfer("/C:/a 'quoted'\nfile", None).unwrap(),
        b"hello\0world"
    );
    let mut sent = Client {
        reader: Cursor::new(client.writer),
        writer: Vec::new(),
        id: 0,
    };
    assert_eq!(sent.receive().unwrap(), [1, 0, 0, 0, 3]);
    let open = sent.receive().unwrap();
    assert_eq!(
        Client::<Cursor<Vec<u8>>, Vec<u8>>::value(&open, 3).unwrap(),
        b"/C:/a 'quoted'\nfile"
    );
    let read = sent.receive().unwrap();
    assert_eq!(read[0], 5);
    assert_eq!(&read[read.len() - 12..read.len() - 4], &0u64.to_be_bytes());
    assert_eq!(remote_path(&RemotePath::Home("a*~b".into())), "./a*~b");
}

#[test]
fn write_chunks_and_close_are_acknowledged() {
    let mut input = server_prefix();
    for id in 2..=4 {
        input.extend(packet(101, id, &0u32.to_be_bytes()));
    }
    let bytes = vec![b'x'; CHUNK + 1];
    let mut client = Client {
        reader: Cursor::new(input),
        writer: Vec::new(),
        id: 0,
    };
    assert!(client.transfer("./file", Some(&bytes)).unwrap().is_empty());
    let mut sent = Client {
        reader: Cursor::new(client.writer),
        writer: Vec::new(),
        id: 0,
    };
    sent.receive().unwrap();
    let open = sent.receive().unwrap();
    assert_eq!(
        &open[open.len() - 8..open.len() - 4],
        &0x1au32.to_be_bytes()
    );
    let first = sent.receive().unwrap();
    let second = sent.receive().unwrap();
    assert_eq!(first[0], 6);
    assert_eq!(second[0], 6);
    assert_eq!(
        &second[second.len() - 13..second.len() - 5],
        &(CHUNK as u64).to_be_bytes()
    );
    assert_eq!(sent.receive().unwrap()[0], 4);
}

#[test]
fn invalid_packets_and_failed_write_are_errors() {
    let mut input = server_prefix();
    input.extend(packet(101, 2, &3u32.to_be_bytes()));
    let mut client = Client {
        reader: Cursor::new(input),
        writer: Vec::new(),
        id: 0,
    };
    assert!(
        client
            .transfer("file", Some(b"x"))
            .unwrap_err()
            .contains("status 3")
    );
    let mut client = Client {
        reader: Cursor::new(u32::MAX.to_be_bytes()),
        writer: Vec::new(),
        id: 0,
    };
    assert!(client.receive().unwrap_err().contains("packet limit"));
    let mut client = Client {
        reader: Cursor::new(packet(101, 7, &0u32.to_be_bytes())),
        writer: Vec::new(),
        id: 0,
    };
    assert!(client.request(4, b"").unwrap_err().contains("id mismatch"));
}

#[test]
fn read_aborts_at_one_mib_instead_of_truncating() {
    let mut input = server_prefix();
    let mut chunk = Vec::new();
    string(&mut chunk, &vec![b'x'; CHUNK]);
    for id in 2..=34 {
        input.extend(packet(103, id, &chunk));
    }
    let mut client = Client {
        reader: Cursor::new(input),
        writer: Vec::new(),
        id: 0,
    };
    assert_eq!(
        client.transfer("large", None).unwrap_err(),
        "remote file exceeds 1 MiB"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn openssh_server_round_trip_preserves_binary_bytes_and_literal_paths() {
    use std::process::Command;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a 'quoted' $literal.bin");
    let contents: Vec<u8> = (0..LIMIT).map(|i| (i % 251) as u8).collect();
    let invoke = |contents: Option<&[u8]>| {
        let mut child = Command::new("/usr/libexec/sftp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("macOS includes the OpenSSH SFTP subsystem server");
        let result = {
            let mut client = Client {
                reader: child.stdout.take().unwrap(),
                writer: child.stdin.take().unwrap(),
                id: 0,
            };
            client.transfer(path.to_str().unwrap(), contents)
        };
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        if result.is_ok() {
            assert!(status.success(), "{status}");
        }
        result
    };
    assert!(invoke(Some(&contents)).unwrap().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), contents);
    assert_eq!(invoke(None).unwrap(), contents);
    assert!(invoke(Some(b"short")).unwrap().is_empty());
    assert_eq!(invoke(None).unwrap(), b"short");
    assert!(invoke(Some(b"")).unwrap().is_empty());
    assert!(invoke(None).unwrap().is_empty());
}
