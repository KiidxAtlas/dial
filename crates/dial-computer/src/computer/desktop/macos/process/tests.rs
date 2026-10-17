use super::*;

#[test]
fn computer_tk_images_match_and_neighbours_do_not() {
    for path in [
        "/Library/Frameworks/Tk.framework/Versions/8.6/Tk",
        "/opt/homebrew/Cellar/tcl-tk/9.0.2/lib/libtcl9tk9.0.dylib",
        "/opt/homebrew/opt/tcl-tk@8/lib/libtk8.6.dylib",
        "/usr/local/lib/python3.12/lib-dynload/_tkinter.cpython-312-darwin.so",
    ] {
        assert!(is_tk_image(path), "{path}");
    }
    for path in [
        "",
        "/opt/homebrew/lib/libtcl9.0.dylib",
        "/opt/homebrew/lib/libtkrzw.dylib",
        "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit",
        "/usr/lib/python3/_tkinter_helper.py",
        "/Applications/Foo.app/Contents/MacOS/tk8.6",
    ] {
        assert!(!is_tk_image(path), "{path}");
    }
}

#[test]
fn computer_chromium_identity_does_not_match_unrelated_display_names() {
    assert!(is_chromium_bundle("com.google.Chrome.canary"));
    assert!(is_chromium_bundle("company.thebrowser.Browser"));
    assert!(!is_chromium_bundle("com.apple.archiveutility"));
    assert!(!is_chromium_bundle("com.google.ChromeNotABrowser"));
}

#[test]
fn computer_region_record_matches_kernel_layout() {
    assert_eq!(mem::size_of::<ProcRegionInfo>(), 96);
    assert_eq!(mem::size_of::<ProcRegionWithPathInfo>(), 1272);
}
