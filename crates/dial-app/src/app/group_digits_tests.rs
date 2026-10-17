#[test]
fn digits_are_grouped_in_thousands() {
    assert_eq!(super::group_digits(0), "0");
    assert_eq!(super::group_digits(999), "999");
    assert_eq!(super::group_digits(32768), "32,768");
    assert_eq!(super::group_digits(1_048_576), "1,048,576");
}
