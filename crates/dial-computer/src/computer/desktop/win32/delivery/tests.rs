use super::*;

#[test]
fn recognizes_real_classes_and_rejects_lookalikes() {
    assert!(is_chromium_class("Chrome_WidgetWin_1"));
    assert!(is_chromium_class("CefBrowserWindow"));
    assert!(is_chromium_class("Chrome_RenderWidgetHostHWND"));
    assert!(!is_chromium_class("Chrome_WidgetWin_"));
    assert!(!is_chromium_class("ChromeWidgetWin_1"));

    assert!(is_winui3_class("WinUIDesktopWin32WindowClass"));
    assert!(!is_winui3_class("WinUIDesktopWin32WindowClass2"));

    assert!(is_wpf_class("HwndWrapper[App;;abc]"));
    assert!(!is_wpf_class("HwndWrapper[App;;abc"));
    assert!(!is_wpf_class("HwndWrapperApp;;abc]"));

    assert!(is_tk_class("TkTopLevel.1"));
    assert!(is_tk_class("TkTopLevel"));
    assert!(!is_tk_class("TkTopLevelish"));
    assert!(!is_tk_class("TkTopLevel."));

    assert!(is_gtk_class("gdkSurfaceToplevel"));
    assert!(is_gtk_class("gdkWindowToplevel"));
    assert!(is_gtk_class("gdkWindowTemp"));
    assert!(!is_gtk_class("gdkWindow"));
    assert!(!is_gtk_class("GdkWindowToplevel"));

    assert!(is_vcl_class("SALFRAME"));
    assert!(!is_vcl_class("SAL"));
    assert!(!is_vcl_class("XSALFRAME"));
}

fn assert_matrix(target: TargetTraits<'_>, expected: [bool; 6]) {
    let kinds = [
        EventKind::MouseClick,
        EventKind::MouseMove,
        EventKind::MouseScroll,
        EventKind::Keystroke,
        EventKind::KeyCombo,
        EventKind::TextInput,
    ];
    for (kind, expected) in kinds.into_iter().zip(expected) {
        assert_eq!(
            would_be_silently_dropped(target, kind).is_some(),
            expected,
            "unexpected {target:?}/{} delivery decision",
            kind.name(),
        );
    }
}

const fn background(class: &str) -> TargetTraits<'_> {
    TargetTraits {
        class,
        chromium_descendant: false,
        foreground: false,
        xaml_host: false,
    }
}

#[test]
fn covers_the_full_known_silent_drop_matrix() {
    assert_matrix(background("Chrome_WidgetWin_1"), [true; 6]);
    assert_matrix(background("CefBrowserWindow"), [true; 6]);
    assert_matrix(background("Chrome_RenderWidgetHostHWND"), [true; 6]);
    assert_matrix(
        background("WinUIDesktopWin32WindowClass"),
        [true, true, true, false, false, false],
    );
    assert_matrix(
        background("HwndWrapper[App;;abc]"),
        [true, true, false, true, true, true],
    );
    assert_matrix(
        background("TkTopLevel.1"),
        [true, false, false, true, true, true],
    );
    assert_matrix(
        background("gdkSurfaceToplevel"),
        [true, false, false, false, false, false],
    );
    assert_matrix(
        background("gdkWindowToplevel"),
        [true, false, false, false, false, false],
    );
    assert_matrix(
        background("SALFRAME"),
        [false, false, false, true, true, false],
    );
    assert_matrix(background("Chrome_WidgetWin"), [false; 6]);
    assert_matrix(background("HwndWrapperApp;;abc]"), [false; 6]);
    assert_matrix(background("TkTopLevelish"), [false; 6]);
    assert_matrix(background("XSALFRAME"), [false; 6]);
}

#[test]
fn wpf_accepts_posted_keys_only_while_it_owns_the_foreground() {
    let foreground = TargetTraits {
        foreground: true,
        ..background("HwndWrapper[App;;abc]")
    };
    assert_matrix(foreground, [true, true, false, false, false, true]);
    let native = TargetTraits {
        foreground: true,
        ..background("Notepad")
    };
    assert_matrix(native, [false; 6]);
}

#[test]
fn xaml_hosts_refuse_posted_keyboard_and_uwp_frames_refuse_posted_pointer() {
    let notepad = TargetTraits {
        xaml_host: true,
        ..background("Notepad")
    };
    assert_matrix(notepad, [false, false, false, true, true, true]);
    let uwp = TargetTraits {
        xaml_host: true,
        ..background("ApplicationFrameWindow")
    };
    assert_matrix(uwp, [true, true, false, true, true, true]);
    let foreground_uwp = TargetTraits {
        foreground: true,
        ..uwp
    };
    assert_matrix(foreground_uwp, [true, true, false, true, true, true]);
}

#[test]
fn terminals_accept_posted_text_only_while_foreground() {
    assert_matrix(
        background("CASCADIA_HOSTING_WINDOW_CLASS"),
        [false, false, false, false, false, true],
    );
    assert_matrix(
        background("mintty"),
        [false, false, false, false, false, true],
    );
    let foreground = TargetTraits {
        foreground: true,
        ..background("ConsoleWindowClass")
    };
    assert_matrix(foreground, [false; 6]);
}

#[test]
fn only_arm64_console_host_rejects_text_in_every_mode() {
    assert!(text_input_unsupported("ConsoleWindowClass", true).is_some());
    assert!(text_input_unsupported("ConsoleWindowClass", false).is_none());
    assert!(text_input_unsupported("CASCADIA_HOSTING_WINDOW_CLASS", true).is_none());
}

#[test]
fn embedded_chromium_refuses_drags_wheel_and_chords_on_top_of_host_toolkit_drops() {
    let tauri = TargetTraits {
        chromium_descendant: true,
        ..background("Tauri Window")
    };
    assert_matrix(tauri, [false, true, true, false, true, false]);
    let wpf_host = TargetTraits {
        chromium_descendant: true,
        ..background("HwndWrapper[App;;x]")
    };
    assert_matrix(wpf_host, [true; 6]);
}

#[test]
fn only_caption_and_sizing_hits_refuse_posted_drags() {
    assert_eq!(non_client_drag_region(2), Some("caption"));
    assert_eq!(non_client_drag_region(4), Some("size box"));
    for border in 10..=17 {
        assert_eq!(non_client_drag_region(border), Some("resize border"));
    }
    // HTERROR, HTNOWHERE, HTCLIENT, HTSYSMENU, HTMINBUTTON, HTBORDER, HTCLOSE
    for hit in [-2, 0, 1, 3, 8, 18, 20] {
        assert_eq!(non_client_drag_region(hit), None, "hit {hit}");
    }
}

#[test]
fn double_click_messages_follow_class_style_and_press_parity() {
    let presses = |wants_double| (0..4).map(move |index| posts_double_click(index, wants_double));
    assert!(presses(true).eq([false, true, false, true]));
    assert!(presses(false).eq([false; 4]));
}

#[test]
fn every_line_break_style_becomes_exactly_one_return() {
    use TextUnit::{Char, Enter};
    let units = |text| text_units(text).collect::<Vec<_>>();
    assert_eq!(
        units("a\r\nb\nc\rd"),
        [
            Char('a'),
            Enter,
            Char('b'),
            Enter,
            Char('c'),
            Enter,
            Char('d')
        ]
    );
    assert_eq!(units("\r\r\n\n"), [Enter, Enter, Enter]);
    assert_eq!(units("\n\r"), [Enter, Enter]);
    assert_eq!(units("é\t"), [Char('é'), Char('\t')]);
}
