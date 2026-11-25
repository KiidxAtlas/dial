use super::*;

#[test]
fn argument_range_counts_fields_after_the_last_parenthesis() {
    let mut fields: Vec<String> = (3..=52).map(|field| field.to_string()).collect();
    fields[48 - 3] = "1000".into();
    fields[49 - 3] = "1200".into();
    let stat = format!("42 (odd) name)) {}", fields.join(" "));
    assert_eq!(argument_range(&stat), Some((1000, 1200)));
    assert_eq!(argument_range("42 (dial) S 1"), None);
}

#[test]
fn only_whole_matching_arguments_are_blanked() {
    let mut bytes = b"dial\0-p\0start qemu now\0start qemu\0".to_vec();
    assert_eq!(blank(&mut bytes, &["start qemu now", ""]), 1);
    assert_eq!(&bytes, b"dial\0-p\0              \0start qemu\0");
    // An unterminated final argument is matched too; an empty text never blanks anything.
    let mut tail = b"dial\0prompt".to_vec();
    assert_eq!(blank(&mut tail, &["prompt"]), 1);
    assert_eq!(&tail, b"dial\0      ");
}

#[test]
fn blanking_nothing_leaves_the_command_line_unchanged() {
    let before: Vec<_> = std::env::args_os().collect();
    assert_eq!(
        blank_arguments(&["\u{1}no argument has this text\u{1}"]).unwrap(),
        0
    );
    assert_eq!(std::env::args_os().collect::<Vec<_>>(), before);
}
