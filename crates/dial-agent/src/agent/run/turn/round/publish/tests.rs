use super::{elapsed_note, format_elapsed};

#[test]
fn long_running_tools_always_report_elapsed_time() {
    assert_eq!(
        elapsed_note("eval", 0.42).as_deref(),
        Some("\n[elapsed 0.4s]")
    );
    assert_eq!(
        elapsed_note("bash", 246.2).as_deref(),
        Some("\n[elapsed 4m06s]")
    );
}

#[test]
fn other_tools_report_elapsed_time_only_when_slow() {
    assert_eq!(elapsed_note("read", 0.3), None);
    assert_eq!(elapsed_note("grep", 4.9), None);
    assert_eq!(
        elapsed_note("grep", 12.0).as_deref(),
        Some("\n[elapsed 12s]")
    );
}

#[test]
fn shared_or_unmeasured_results_carry_no_time() {
    assert_eq!(elapsed_note("eval", 0.0), None);
    assert_eq!(elapsed_note("eval", f64::NAN), None);
}

#[test]
fn elapsed_time_formats_by_magnitude() {
    assert_eq!(format_elapsed(5.6), "5.6s");
    assert_eq!(format_elapsed(9.96), "10s");
    assert_eq!(format_elapsed(59.4), "59s");
    assert_eq!(format_elapsed(60.0), "1m00s");
    assert_eq!(format_elapsed(3725.0), "1h02m");
}
