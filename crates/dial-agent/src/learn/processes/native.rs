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

fn token_user(process: HANDLE) -> Result<Vec<usize>> {
    let mut token = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0,
        "{}",
        std::io::Error::last_os_error()
    );
    let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    let mut length = 0;
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
    let mut bytes = vec![0usize; (length as usize).div_ceil(mem::size_of::<usize>())];
    ensure!(
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
    Ok(bytes)
}

pub(super) fn current_user() -> Result<Vec<usize>> {
    token_user(unsafe { GetCurrentProcess() })
}

pub(super) struct NativeProcess(OwnedHandle);
impl NativeProcess {
    pub(super) fn open(pid: u32, for_stop: bool) -> Result<Self> {
        let access =
            PROCESS_QUERY_LIMITED_INFORMATION | if for_stop { PROCESS_TERMINATE } else { 0 };
        let handle = unsafe { OpenProcess(access, 0, pid) };
        ensure!(
            !handle.is_null(),
            "opening process {pid}: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.cast()) }))
    }

    pub(super) fn handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }

    pub(super) fn owned_by(&self, user: &[usize]) -> Result<bool> {
        let target = token_user(self.0.as_raw_handle().cast())?;
        let user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
        let target = unsafe { &*target.as_ptr().cast::<TOKEN_USER>() };
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
