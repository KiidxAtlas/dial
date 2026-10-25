use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows_sys::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};

#[derive(Debug)]
pub(super) struct Request {
    handle: OwnedHandle,
}

impl Request {
    pub(super) fn acquire() -> Result<Self> {
        let mut reason: Vec<u16> = "Dial active agent work".encode_utf16().chain([0]).collect();
        let context = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr(),
            },
        };
        // SAFETY: context and its NUL-terminated UTF-16 string remain live during
        // this copying call. The resulting kernel handle is not thread-affine.
        let raw = unsafe { PowerCreateRequest(&context) };
        if raw == INVALID_HANDLE_VALUE || raw.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("creating idle sleep power request");
        }
        // SAFETY: raw is a newly created valid handle, uniquely owned here. OwnedHandle
        // closes it even if PowerSetRequest fails, and can safely move between threads.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: handle is a live power-request handle owned by this scope.
        if unsafe { PowerSetRequest(handle.as_raw_handle(), PowerRequestSystemRequired) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("enabling idle sleep power request");
        }
        Ok(Self { handle })
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: the handle stays live until OwnedHandle drops after this method.
        // Clearing only this handle cannot cancel another guard's request.
        unsafe { PowerClearRequest(self.handle.as_raw_handle(), PowerRequestSystemRequired) };
    }
}
