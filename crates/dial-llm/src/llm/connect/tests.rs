use super::*;

#[test]
fn frames_wait_for_complete_headers_and_payloads() {
    let wire = frame(0, b"hello").unwrap();
    let mut frames = Frames::default();
    assert!(frames.push(&wire[..3]).unwrap().is_empty());
    assert!(frames.push(&wire[3..8]).unwrap().is_empty());
    let out = frames.push(&wire[8..]).unwrap();
    assert_eq!(out, vec![(0, b"hello".to_vec())]);
}

#[test]
fn oversized_frame_header_is_rejected_before_buffering() {
    let mut header = vec![0u8];
    header.extend_from_slice(&((MAX_FRAME_PAYLOAD as u32) + 1).to_be_bytes());
    assert!(Frames::default().push(&header).is_err());
}

#[test]
fn frame_reports_flags_and_length_and_rejects_nothing_small() {
    let wire = frame(FLAG_END_STREAM, b"{}").unwrap();
    assert_eq!(wire[0], FLAG_END_STREAM);
    assert_eq!(&wire[1..5], &2u32.to_be_bytes());
    assert_eq!(&wire[5..], b"{}");
}

#[test]
fn gzip_round_trips_and_gunzip_rejects_plain_bytes() {
    let packed = gzip(b"payload").unwrap();
    assert_eq!(gunzip(&packed).unwrap(), b"payload");
    assert!(gunzip(b"plain").is_err());
}

#[test]
fn protobuf_fields_round_trip_scalars_strings_and_nested_messages() {
    let mut inner = Enc::default();
    inner.string(1, "text");
    let mut outer = Enc::default();
    outer.uint(1, 300);
    outer.flag(2, true);
    outer.double(3, 1.5);
    outer.string(4, "name");
    outer.message(5, &inner);
    let fields = Fields::parse(&outer.0).unwrap();
    assert_eq!(fields.uint(1), 300);
    assert_eq!(fields.uint(2), 1);
    assert_eq!(fields.fixed64_f64(3), 1.5);
    assert_eq!(fields.string(4), "name");
    assert_eq!(fields.message(5).unwrap().unwrap().string(1), "text");
    assert_eq!(fields.string(9), "");
}

#[test]
fn truncated_protobuf_is_an_error_not_a_panic() {
    let mut enc = Enc::default();
    enc.string(1, "abcdef");
    assert!(Fields::parse(&enc.0[..enc.0.len() - 2]).is_err());
    assert!(take_varint(&mut &[0x80u8, 0x80][..]).is_err());
}

#[test]
fn optional_uint_distinguishes_missing_usage_from_explicit_zero() {
    let fields = Fields::parse(&[8, 0]).unwrap();
    assert_eq!(fields.uint_opt(1), Some(0));
    assert_eq!(fields.uint_opt(2), None);
    assert_eq!(fields.uint(2), 0);
}
