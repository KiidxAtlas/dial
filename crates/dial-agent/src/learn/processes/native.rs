//! Queries and controls retain the same native process handle, including across identity checks.
use super::Process;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    mem,
    os::windows::{
        ffi::OsStringExt,
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
};
use windows_sys::Win32::{
    Foundation::{FILETIME, HANDLE},
    Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser},
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
        },
    },
};

/// An aligned TokenUser allocation, including the SID that its TOKEN_USER header points into.
/// The allocation is never cloned or resized: moving the owner leaves the SID address stable.
pub(super) struct UserToken(Vec<usize>);

fn token_user(process: HANDLE) -> Result<UserToken> {
    let mut token = ptr::null_mut();
    ensure!(
        // SAFETY: `token` is writable; the OS validates the process handle. On success the token
        // handle is transferred exactly once into OwnedHandle below.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: OpenProcessToken succeeded and returned a new owned, non-null token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    let mut length = 0;
    // SAFETY: the null/zero buffer pair requests only the size; `length` is writable.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut length,
        );
    }
    ensure!(
        length as usize >= mem::size_of::<TOKEN_USER>(),
        "reading process token: {}",
        std::io::Error::last_os_error()
    );
    // TOKEN_USER contains a pointer and u32; usize storage provides its alignment on Windows.
    // Keep the complete allocation because the returned SID points inside it.
    let mut bytes = vec![0usize; (length as usize).div_ceil(mem::size_of::<usize>())];
    ensure!(
        // SAFETY: the initialized allocation has at least `length` bytes with TOKEN_USER
        // alignment; the live token initializes the header and embedded SID on success.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle().cast(),
                TokenUser,
                bytes.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } != 0,
        "reading process token: {}",
        std::io::Error::last_os_error()
    );
    Ok(UserToken(bytes))
}

pub(super) fn current_user() -> Result<UserToken> {
    // SAFETY: GetCurrentProcess takes no arguments and returns a borrowed pseudo-handle. It is
    // passed only to OpenProcessToken, never adopted into OwnedHandle or closed.
    token_user(unsafe { GetCurrentProcess() })
}

pub(super) struct NativeProcess(OwnedHandle);
impl NativeProcess {
    pub(super) fn open(pid: u32, for_stop: bool) -> Result<Self> {
        let access =
            PROCESS_QUERY_LIMITED_INFORMATION | if for_stop { PROCESS_TERMINATE } else { 0 };
        // SAFETY: scalar arguments only; the OS validates the PID/access mask. A successful
        // non-null result is a new handle which is immediately owned below.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        ensure!(
            !handle.is_null(),
            "opening process {pid}: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the non-null handle is owned by this call and has not been adopted elsewhere.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.cast()) }))
    }

    pub(super) fn handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }

    pub(super) fn owned_by(&self, user: &UserToken) -> Result<bool> {
        let target = token_user(self.0.as_raw_handle().cast())?;
        // SAFETY: only token_user constructs these owners from successful TokenUser queries.
        // Both aligned allocations retain their headers and embedded SIDs without relocation.
        let (user, target) = unsafe {
            (
                &*user.0.as_ptr().cast::<TOKEN_USER>(),
                &*target.0.as_ptr().cast::<TOKEN_USER>(),
            )
        };
        // SAFETY: both SID pointers refer to OS-initialized SIDs in the still-live owners above.
        Ok(unsafe { EqualSid(user.User.Sid, target.User.Sid) } != 0)
    }

    pub(super) fn inspect(&self, pid: i32, parent: i32) -> Result<(Process, u64, u64)> {
        let handle = self.0.as_raw_handle().cast();
        let (mut created, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        ensure!(
            // SAFETY: this OwnedHandle remains live; all four FILETIME out-pointers are writable.
            unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) }
                != 0,
            "reading process times: {}",
            std::io::Error::last_os_error()
        );
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        let mut image = vec![0u16; 32768];
        let mut length = image.len() as u32;
        ensure!(
            // SAFETY: `image` has `length` writable UTF-16 units and the process handle is live.
            unsafe { QueryFullProcessImageNameW(handle, 0, image.as_mut_ptr(), &mut length) } != 0,
            "reading executable image: {}",
            std::io::Error::last_os_error()
        );
        let command = OsString::from_wide(&image[..length as usize])
            .to_string_lossy()
            .into_owned();
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        // SAFETY: `counters` is initialized with its exact size and is writable for the call;
        // the process handle remains live. Failure leaves memory unavailable, not uninitialized.
        let memory = (unsafe {
            GetProcessMemoryInfo(handle, &mut counters, mem::size_of_val(&counters) as u32)
        } != 0)
            .then_some(counters.WorkingSetSize as u64);
        let cpu_ticks = ticks(kernel)
            .checked_add(ticks(user))
            .context("process CPU time overflow")?;
        let born = ticks(created);
        Ok((
            Process {
                pid,
                parent,
                cpu: None,
                memory,
                started: born.to_string(),
                command,
                id: None,
            },
            cpu_ticks,
            born,
        ))
    }
}
