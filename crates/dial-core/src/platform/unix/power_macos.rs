use std::ffi::{c_char, c_void};

use anyhow::{Result, ensure};

type CfString = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        text: *const c_char,
        encoding: u32,
    ) -> CfString;
    fn CFRelease(value: *const c_void);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CfString,
        level: u32,
        name: CfString,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    #[cfg(test)]
    fn IOPMAssertionCopyProperties(assertion_id: u32) -> *const c_void;
}

struct StringRef(CfString);

impl StringRef {
    fn new(text: &std::ffi::CStr) -> Result<Self> {
        // kCFStringEncodingUTF8, as defined by CoreFoundation.
        const UTF8: u32 = 0x0800_0100;
        // SAFETY: text is NUL-terminated and lives through this copying call; null
        // allocator selects the default allocator.
        let value = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
        ensure!(
            !value.is_null(),
            "could not allocate power assertion string"
        );
        Ok(Self(value))
    }
}

impl Drop for StringRef {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of a non-null Create-rule CF object.
        unsafe { CFRelease(self.0) };
    }
}

#[derive(Debug)]
pub(super) struct Request {
    id: u32,
}

impl Request {
    pub(super) fn acquire() -> Result<Self> {
        let kind = StringRef::new(c"PreventUserIdleSystemSleep")?;
        let reason = StringRef::new(c"Dial active agent work")?;
        let mut id = 0;
        // SAFETY: both CF strings are live, and id is writable. IOKit copies the
        // strings. Level 255 is kIOPMAssertionLevelOn; the ID is not thread-bound.
        let status = unsafe { IOPMAssertionCreateWithName(kind.0, 255, reason.0, &mut id) };
        ensure!(status == 0, "IOPM idle sleep assertion failed: {status:#x}");
        Ok(Self { id })
    }

    #[cfg(test)]
    pub(super) fn id(&self) -> u32 {
        self.id
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: id is the assertion owned by this request and released only here.
        unsafe { IOPMAssertionRelease(self.id) };
    }
}

#[cfg(test)]
pub(super) fn assertion_exists(id: u32) -> bool {
    // SAFETY: any assertion ID is accepted; absent IDs return null. A non-null
    // result follows the Copy rule and must be released by the caller.
    let properties = unsafe { IOPMAssertionCopyProperties(id) };
    if properties.is_null() {
        return false;
    }
    // SAFETY: properties is a non-null owned CF dictionary from the Copy call.
    unsafe { CFRelease(properties) };
    true
}
