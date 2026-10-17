use super::error::ErrorCode;
use super::*;

/// A slim build has no native backend: desktop operations must fail with `Unsupported` and
/// say why, while capabilities report nothing as available.
#[test]
fn slim_build_fails_desktop_operations_without_claiming_capability() {
    let session = DesktopSession::new(None).expect("construction does not touch the platform");
    let caps = session
        .capabilities()
        .expect("capabilities report unavailability");
    assert_eq!(caps.backend, "unavailable");
    assert!(!caps.capture && !caps.input && !caps.ax && !caps.applications);
    assert_eq!(caps.display_count, 0);

    let Err(error) = session.list_displays() else {
        panic!("slim build must not list displays");
    };
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(
        error.message.contains("desktop feature is disabled"),
        "{error}"
    );

    let Err(error) = session.list_applications(None) else {
        panic!("slim build must not list applications");
    };
    assert_eq!(error.code, ErrorCode::Unsupported);
}
