use super::human_bytes;

#[test]
fn human_bytes_uses_the_largest_binary_unit_below_1024() {
    let cases = [
        (0, "0 B"),
        (512, "512 B"),
        (1023, "1023 B"),
        (1024, "1.0 KiB"),
        (1536, "1.5 KiB"),
        (3 * 1024 * 1024, "3.0 MiB"),
        (5 * 1024 * 1024 * 1024, "5.0 GiB"),
        (1024_u64.pow(4), "1.0 TiB"),
        (u64::MAX, "16777216.0 TiB"),
    ];
    for (bytes, expected) in cases {
        assert_eq!(human_bytes(bytes), expected, "bytes={bytes}");
    }
}
