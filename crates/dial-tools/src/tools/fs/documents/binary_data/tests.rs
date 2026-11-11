use super::*;

fn npy_file(descr: &str, shape: &str, data: &[u8]) -> Vec<u8> {
    let mut header = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape}, }}");
    while (10 + header.len() + 1) % 64 != 0 {
        header.push(' ');
    }
    header.push('\n');
    let mut out = b"\x93NUMPY\x01\x00".to_vec();
    out.extend_from_slice(&(header.len() as u16).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

#[test]
fn npy_reports_dtype_shape_and_values() {
    let data: Vec<u8> = [1.5f64, -2.0, 3.25]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let out = npy(&npy_file("<f8", "(3,)", &data)).unwrap();
    assert!(out.contains("dtype <f8, shape (3), 3 element(s)"), "{out}");
    assert!(out.contains("1.5  -2  3.25"), "{out}");

    let data: Vec<u8> = (0i32..6).flat_map(|v| (v - 2).to_be_bytes()).collect();
    let out = npy(&npy_file(">i4", "(2, 3)", &data)).unwrap();
    assert!(out.contains("shape (2 x 3)"), "{out}");
    assert!(
        out.contains("[0] -2  -1  0") && out.contains("[1] 1  2  3"),
        "{out}"
    );

    let out = npy(&npy_file("|O", "(1,)", &[0; 8])).unwrap();
    assert!(out.contains("not decoded"), "{out}");
    assert!(npy(b"\x93NUMPY\x01\x00\xff\xff").is_err());
}

#[test]
fn msgpack_decodes_values_and_streams() {
    // {"a": 1, "b": [true, nil, -1, 1.5], "c": bin(2)}
    let one = b"\x83\xa1a\x01\xa1b\x94\xc3\xc0\xff\xcb\x3f\xf8\0\0\0\0\0\0\xa1c\xc4\x02\x00\x01";
    let out = msgpack(one).unwrap();
    let json: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
    assert_eq!(
        json,
        json!({"a":1,"b":[true,null,-1,1.5],"c":"[binary 2 bytes]"})
    );
    let out = msgpack(b"\x01\x92\x02\x03").unwrap();
    assert!(
        out.contains("2 object(s)") && out.contains("\n1\n[2,3]\n"),
        "{out}"
    );
    assert!(msgpack(b"\x92\x01").unwrap_err().contains("truncated"));
    assert!(msgpack(&[0x91; 100]).unwrap_err().contains("nesting"));
}

fn sfnt(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 1, 0, 0];
    out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    let mut offset = 12 + 16 * tables.len();
    for (tag, body) in tables {
        out.extend_from_slice(*tag);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        offset += body.len();
    }
    for (_, body) in tables {
        out.extend_from_slice(body);
    }
    out
}

#[test]
fn font_names_metrics_and_tables() {
    let family: Vec<u8> = "Probe Sans"
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    let mut name = vec![0, 0, 0, 1, 0, 18];
    for v in [3u16, 1, 0x0409, 1, family.len() as u16, 0] {
        name.extend_from_slice(&v.to_be_bytes());
    }
    name.extend_from_slice(&family);
    let mut head = vec![0; 54];
    head[18..20].copy_from_slice(&2048u16.to_be_bytes());
    let mut maxp = vec![0; 6];
    maxp[4..6].copy_from_slice(&321u16.to_be_bytes());
    let out = font(&sfnt(&[(b"head", head), (b"maxp", maxp), (b"name", name)])).unwrap();
    for expected in [
        "TrueType font",
        "family: Probe Sans",
        "units per em: 2048",
        "glyphs: 321",
        "tables (3): head maxp name",
    ] {
        assert!(out.contains(expected), "{expected}\n{out}");
    }
    assert!(font(b"nope").is_err());
}

fn wav_file(samples: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    for v in [1u16, 1] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    for v in [2u16, 16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

#[test]
fn wav_format_duration_and_levels() {
    let out = wav(&wav_file(&[16384, -16384, 16384, -16384])).unwrap();
    assert!(out.contains("PCM, 1 channel(s), 8000 Hz, 16-bit"), "{out}");
    assert!(out.contains("duration: 0.001 s (8 data bytes)"), "{out}");
    assert!(
        out.contains("peak -6.0 dBFS, RMS -6.0 dBFS over 4 sample(s)"),
        "{out}"
    );
    assert!(wav(&wav_file(&[0; 8])).unwrap().contains("digital silence"));
    assert!(wav(b"RIFF\0\0\0\0WAVE").unwrap_err().contains("fmt"));
}

#[test]
fn read_routes_by_extension_and_by_magic_bytes() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let data: Vec<u8> = [7u8, 8, 9].to_vec();
    std::fs::write(
        dir.path().join("weights.npy"),
        npy_file("|u1", "(3,)", &data),
    )
    .unwrap();
    std::fs::write(dir.path().join("capture"), wav_file(&[0; 4])).unwrap();
    std::fs::write(dir.path().join("state.msgpack"), b"\x81\xa1k\x2a").unwrap();
    let read = |path: &str| crate::tools::execute(&ws, "read", &json!({ "path": path }), 30);
    let npy = read("weights.npy");
    assert!(
        !npy.is_error && npy.text.contains("7  8  9"),
        "{}",
        npy.text
    );
    let wav = read("capture");
    assert!(
        !wav.is_error && wav.text.contains("WAV audio: PCM"),
        "{}",
        wav.text
    );
    let msgpack = read("state.msgpack");
    assert!(
        !msgpack.is_error && msgpack.text.contains("\"k\": 42"),
        "{}",
        msgpack.text
    );
}
