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

#[test]
fn incomplete_frame_eof_is_retryable() {
    let wire = frame(0, b"payload").unwrap();
    for end in 1..wire.len() {
        let mut frames = Frames::default();
        frames.push(&wire[..end]).unwrap();
        let error = frames.finish().unwrap_err();
        assert_eq!(
            super::super::errors::classify(&error),
            super::super::errors::ErrorClass::Transient
        );
    }
    let mut frames = Frames::default();
    frames.push(&wire).unwrap();
    frames.finish().unwrap();
}

#[test]
fn gzip_expansion_is_bounded_and_exact_limit_is_valid() {
    let packed = gzip(&vec![b'x'; MAX_FRAME_PAYLOAD + 1]).unwrap();
    assert!(gunzip(&packed).unwrap_err().to_string().contains("cap"));
    let packed = gzip(b"1234").unwrap();
    assert_eq!(gunzip_limited(&packed, 4).unwrap(), b"1234");
    assert!(gunzip_limited(&packed, 3).is_err());
}

#[test]
fn decoded_response_accounting_rejects_cumulative_expansion() {
    let mut total = super::super::MAX_STREAM_BYTES - 3;
    account_payload(&mut total, 3).unwrap();
    assert!(account_payload(&mut total, 1).is_err());
    assert_eq!(total, super::super::MAX_STREAM_BYTES);
}
