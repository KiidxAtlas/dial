use std::{
    io::ErrorKind,
    os::unix::net::UnixListener,
    sync::{Mutex, mpsc},
    thread,
};

use super::*;

static LIBEI_ENV_LOCK: Mutex<()> = Mutex::new(());

fn backend_without_services() -> WaylandBackend {
    WaylandBackend {
        display: DisplaySelector::All,
        ax: None,
        ax_error: None,
        input: None,
        displays: Vec::new(),
    }
}
fn with_fake_libei(action: impl FnOnce(&mut WaylandBackend)) -> bool {
    let _guard = LIBEI_ENV_LOCK.lock().expect("lock LIBEI_SOCKET test");
    let socket = std::env::temp_dir().join(format!("dial-libei-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind fake libei socket");
    listener
        .set_nonblocking(true)
        .expect("make fake libei socket nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel();
    let accepted = thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if !matches!(
                        stop_rx.recv_timeout(std::time::Duration::from_millis(10)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return false;
                    }
                }
                Err(err) => panic!("fake libei listener: {err}"),
            }
        }
    });
    let previous = std::env::var_os("LIBEI_SOCKET");
    unsafe { std::env::set_var("LIBEI_SOCKET", &socket) };
    let mut backend = WaylandBackend::new(DisplaySelector::All);
    action(&mut backend);
    let _ = stop_tx.send(());
    if let Some(previous) = previous {
        unsafe { std::env::set_var("LIBEI_SOCKET", previous) };
    } else {
        unsafe { std::env::remove_var("LIBEI_SOCKET") };
    }
    let connected = accepted.join().expect("fake libei listener");
    let _ = std::fs::remove_file(socket);
    connected
}

#[test]
fn readonly_backend_creation_does_not_connect_to_libei() {
    let mut capabilities = None;
    let connected = with_fake_libei(|backend| capabilities = Some(backend.capabilities()));
    assert!(
        !connected,
        "read-only backend construction connected to libei"
    );
    let capabilities = capabilities.expect("Wayland capabilities");
    assert!(capabilities.input);
    assert_eq!(capabilities.input_permission, "prompt-or-granted");
}

#[test]
fn desktop_input_connects_to_libei_lazily() {
    let connected = with_fake_libei(|backend| {
        let _ = backend.type_text(
            &Target::Desktop,
            "hello",
            DeliveryMode::Foreground,
            &control::CancellationSource::default().token(),
        );
    });
    assert!(connected, "desktop input did not connect to libei");
}

#[test]
fn failed_input_request_allows_another_connection_attempt() {
    let connected = with_fake_libei(|backend| {
        let socket = std::env::var_os("LIBEI_SOCKET").expect("fake libei socket");
        unsafe {
            std::env::set_var(
                "LIBEI_SOCKET",
                std::path::Path::new(&socket).with_extension("missing"),
            )
        };
        let first = backend
            .type_text(
                &Target::Desktop,
                "hello",
                DeliveryMode::Foreground,
                &control::CancellationSource::default().token(),
            )
            .expect_err("missing socket must fail");
        assert_eq!(first.code.as_str(), "PermissionDenied");
        let caps = backend.capabilities();
        assert!(caps.input);
        assert_eq!(caps.input_permission, "prompt-or-granted");
        unsafe { std::env::set_var("LIBEI_SOCKET", socket) };
        let _ = backend.type_text(
            &Target::Desktop,
            "hello",
            DeliveryMode::Foreground,
            &control::CancellationSource::default().token(),
        );
    });
    assert!(
        connected,
        "a failed input request prevented the next connection attempt"
    );
}

#[test]
fn display_enumeration_preserves_last_capture_snapshot() {
    let mut backend = backend_without_services();
    backend
        .displays
        .push(PortalGeometry::new(None, None, 1920, 1080).display());
    let displays = backend.displays().unwrap();
    assert_eq!(displays.len(), 1);
    assert_eq!((displays[0].width, displays[0].height), (1920, 1080));
}

#[test]
#[cfg(not(feature = "wayland-pipewire"))]
fn display_validation_never_trusts_cached_portal_geometry() {
    let mut backend = backend_without_services();
    backend
        .displays
        .push(PortalGeometry::new(None, None, 1920, 1080).display());
    let frame = FrameGeometry::for_displays(&backend.displays);
    let error = backend.validate_frame_layout(&frame).unwrap_err();
    assert_eq!(error.code.as_str(), "InvalidCoordinateFrame");
}

#[test]
fn cancelled_mutations_stop_before_initializing_input() {
    let mut backend = backend_without_services();
    let source = control::CancellationSource::default();
    let token = source.token();
    source.cancel();
    assert!(
        backend
            .type_text(&Target::Desktop, "hello", DeliveryMode::Foreground, &token)
            .is_err()
    );
    assert!(backend.input.is_none());
    assert!(
        backend
            .key_chord(
                &Target::Desktop,
                &[KeyName::Enter],
                DeliveryMode::Foreground,
                &token
            )
            .is_err()
    );
    assert!(backend.input.is_none());
}

#[test]
fn window_foreground_delivery_reports_compositor_constraint() {
    let mut backend = backend_without_services();
    let target = Target::Window("w1".to_string());
    let err = backend
        .type_text(
            &target,
            "hello",
            DeliveryMode::Foreground,
            &control::CancellationSource::default().token(),
        )
        .expect_err("window foreground input must fail");
    assert_eq!(err.code.as_str(), "BackgroundUnavailable");
}

#[test]
fn window_raise_reports_compositor_constraint() {
    let mut backend = backend_without_services();
    let err = backend
        .raise_window("w1", &control::CancellationSource::default().token())
        .expect_err("Wayland window raise must fail");
    assert_eq!(err.code.as_str(), "BackgroundUnavailable");
}

#[test]
#[cfg(not(feature = "wayland-pipewire"))]
fn capabilities_report_no_capture_without_pipewire_feature() {
    let mut backend = WaylandBackend {
        display: DisplaySelector::All,
        ax: None,
        ax_error: None,
        input: None,
        displays: Vec::new(),
    };
    let caps = backend.capabilities();
    // Shipped builds compile without wayland-pipewire, so the capture path is
    // absent; capabilities() must not advertise capture the binary cannot do.
    assert!(
        !caps.capture,
        "capture must be false when the pipewire feature is off"
    );
    assert_eq!(caps.capture_permission, "unavailable");
    let err = backend
        .capture(&Target::Desktop, &CaptureCaps::default(), None)
        .expect_err("capture must fail without the pipewire feature");
    assert_eq!(err.code.as_str(), "CaptureFailed");
}

fn portal_window(x: i32, y: i32, width: u32, height: u32, position_known: bool) -> AtSpiWindow {
    AtSpiWindow {
        window: DesktopWindow {
            id: "w".into(),
            title: "T".into(),
            app: "A".into(),
            pid: None,
            x,
            y,
            width,
            height,
            focused: false,
        },
        position_known,
    }
}

#[test]
fn scaled_monitor_maps_screenshot_pixel_to_logical_point() {
    // 2560x2880 buffer for a 1280x1440 logical region at scale 2 (issue
    // #11540).
    let geometry = PortalGeometry::new(Some((0, 0)), Some((1280, 1440)), 2560, 2880);
    let display = geometry.display();
    assert_eq!((display.width, display.height), (1280, 1440));
    assert!((display.scale - 2.0).abs() < f64::EPSILON);
    let frame = FrameGeometry::for_displays(&[display]);
    // A lower-half click that the old identity mapping pushed outside the
    // 1280x1440 input region now lands inside it.
    let (lx, ly) = frame.map_point(1066.0, 1867.0, None).unwrap();
    assert!((lx - 533.0).abs() < 1e-6, "logical x {lx}");
    assert!((ly - 933.5).abs() < 1e-6, "logical y {ly}");
    assert!(
        lx < 1280.0 && ly < 1440.0,
        "mapped point must stay inside the logical region"
    );
}

#[test]
fn monitor_offset_is_added_to_logical_point() {
    let geometry = PortalGeometry::new(Some((100, 50)), Some((1280, 1440)), 2560, 2880);
    let frame = FrameGeometry::for_displays(&[geometry.display()]);
    assert_eq!(
        frame.map_point(1280.0, 1440.0, None).unwrap(),
        (740.0, 770.0)
    );
}

#[test]
fn missing_portal_size_falls_back_to_buffer_scale_one() {
    let geometry = PortalGeometry::new(None, None, 1920, 1080);
    let display = geometry.display();
    assert_eq!((display.x, display.y), (0, 0));
    assert_eq!((display.width, display.height), (1920, 1080));
    assert!((display.scale - 1.0).abs() < f64::EPSILON);
    // Degenerate (zero) portal dimensions take the same fallback.
    let degenerate = PortalGeometry::new(Some((0, 0)), Some((0, 0)), 1920, 1080);
    assert_eq!(degenerate.display().width, 1920);
}

#[test]
fn window_crop_scales_logical_bounds_to_buffer_pixels() {
    let geometry = PortalGeometry::new(Some((0, 0)), Some((1280, 1440)), 2560, 2880);
    let crop = geometry
        .window_crop(&portal_window(100, 200, 300, 400, true))
        .expect("window inside monitor");
    assert_eq!(crop, (200, 400, 600, 800));
}

#[test]
fn window_crop_rejects_window_outside_monitor() {
    let geometry = PortalGeometry::new(Some((0, 0)), Some((1280, 1440)), 2560, 2880);
    for window in [
        portal_window(2000, 0, 100, 100, true),
        portal_window(-10, 0, 100, 100, true),
    ] {
        let err = geometry
            .window_crop(&window)
            .expect_err("window outside monitor");
        assert!(
            err.message.contains("outside the selected portal monitor"),
            "{err}"
        );
    }
}

#[test]
fn window_crop_refuses_unknown_screen_position() {
    // Native Wayland clients report AT-SPI Screen extents of 0,0 wherever
    // the compositor placed them; cropping there captured whatever sat at the
    // monitor's top-left instead of the window (issue #13854).
    let geometry = PortalGeometry::new(Some((0, 0)), Some((1920, 1080)), 1920, 1080);
    let err = geometry
        .window_crop(&portal_window(0, 0, 1159, 896, false))
        .expect_err("unknown position must not be cropped");
    assert_eq!(err.code.as_str(), "CaptureFailed");
    assert!(err.message.contains("no known screen position"), "{err}");
}
