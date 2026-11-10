//! Windows process and socket inspection without external tools.
//!
//! - [`snapshot`] lists processes through a Toolhelp snapshot (pid, parent, threads, image name).
//! - [`details`] reads one process: creation time, working set, executable and, when this user may
//!   read its memory, the command line and working directory from its process parameters (PEB).
//! - [`kill_marked`] terminates a process only when its environment block holds an exact
//!   `NAME=value` entry, checking and terminating through one handle so a reused PID is never hit.
//! - [`listeners`] returns listening TCP and bound UDP endpoints with their owning PID (IP Helper).
//!
//! The PEB layout read here (`RTL_USER_PROCESS_PARAMETERS` fields) is the documented-in-practice
//! layout every Windows version since XP shares; a WOW64 process is read through its native PEB,
//! whose parameters block also describes the 32-bit process.
use std::ffi::c_void;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR},
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
        UDP_TABLE_OWNER_PID,
    },
    Networking::WinSock::{AF_INET, AF_INET6},
    System::{
        Diagnostics::{
            Debug::ReadProcessMemory,
            ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
        },
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_TERMINATE, PROCESS_VM_READ, QueryFullProcessImageNameW, TerminateProcess,
            WaitForSingleObject,
        },
    },
};

use super::procinfo::win;

/// Environment blocks larger than this are treated as unreadable.
const MAX_ENVIRONMENT_BYTES: usize = 1 << 20;
const PAGE: usize = 4096;
/// 100 ns intervals between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

#[cfg(target_pointer_width = "64")]
mod layout {
    pub const PARAMETERS: usize = 0x20;
    pub const CURRENT_DIRECTORY: usize = 0x38;
    pub const COMMAND_LINE: usize = 0x70;
    pub const ENVIRONMENT: usize = 0x80;
    /// Offset of `Buffer` inside `UNICODE_STRING`.
    pub const STRING_BUFFER: usize = 8;
}
#[cfg(target_pointer_width = "32")]
mod layout {
    pub const PARAMETERS: usize = 0x10;
    pub const CURRENT_DIRECTORY: usize = 0x24;
    pub const COMMAND_LINE: usize = 0x40;
    pub const ENVIRONMENT: usize = 0x48;
    pub const STRING_BUFFER: usize = 4;
}

#[repr(C)]
struct BasicInformation {
    exit_status: i32,
    peb: usize,
    affinity_mask: usize,
    base_priority: i32,
    pid: usize,
    parent: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// One Toolhelp process entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub pid: u32,
    pub parent: u32,
    pub threads: u32,
    /// Executable file name without its directory.
    pub image: String,
}

/// What one open process handle reveals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// Creation time as a FILETIME (100 ns since 1601); compare with [`filetime_now`].
    pub created: u64,
    pub exited: bool,
    pub working_set: Option<u64>,
    pub executable: Option<String>,
    /// From the process parameters; `None` without `PROCESS_VM_READ` access.
    pub command_line: Option<String>,
    pub cwd: Option<String>,
}

/// A listening TCP or bound UDP endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    /// `tcp`, `tcp6`, `udp` or `udp6`.
    pub protocol: &'static str,
    pub address: IpAddr,
    pub port: u16,
    pub pid: u32,
}

/// The current time as a FILETIME.
pub fn filetime_now() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_EPOCH_FILETIME + (since.as_nanos() / 100) as u64
}

/// Seconds between a FILETIME and now.
pub fn age_secs(created: u64) -> Option<u64> {
    filetime_now().checked_sub(created).map(|d| d / 10_000_000)
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// Every process the system reports, including those this user cannot open.
pub fn snapshot() -> io::Result<Vec<Entry>> {
    // SAFETY: scalar flags; the returned handle is checked before ownership transfer.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid snapshot handle is uniquely owned from here.
    let snap = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let handle = snap.as_raw_handle() as HANDLE;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: snap keeps the handle live; entry is writable with dwSize set as required.
    let mut more = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while more {
        out.push(Entry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            threads: entry.cntThreads,
            image: wide(&entry.szExeFile),
        });
        // SAFETY: as above; the entry keeps its dwSize between calls.
        more = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    Ok(out)
}

/// Reads exactly `buffer.len()` bytes at `address` of `process`.
fn read_memory(process: HANDLE, address: usize, buffer: &mut [u8]) -> bool {
    let mut read = 0usize;
    // SAFETY: buffer is writable for its full length; the remote address is only dereferenced by
    // the kernel, which fails the call instead of faulting on an invalid range.
    let ok = unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut read,
        )
    };
    ok != 0 && read == buffer.len()
}

fn read_pointer(process: HANDLE, address: usize) -> Option<usize> {
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    read_memory(process, address, &mut bytes).then(|| usize::from_ne_bytes(bytes))
}

/// The remote `UNICODE_STRING` at `address`.
fn read_unicode_string(process: HANDLE, address: usize) -> Option<String> {
    let mut length = [0u8; 2];
    if !read_memory(process, address, &mut length) {
        return None;
    }
    let length = usize::from(u16::from_ne_bytes(length));
    let buffer = read_pointer(process, address + layout::STRING_BUFFER)?;
    if length == 0 || buffer == 0 {
        return Some(String::new());
    }
    let mut bytes = vec![0u8; length];
    if !read_memory(process, buffer, &mut bytes) {
        return None;
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_ne_bytes(pair)).collect();
    Some(String::from_utf16_lossy(&units))
}

/// Address of the remote `RTL_USER_PROCESS_PARAMETERS`.
fn parameters(process: HANDLE) -> Option<usize> {
    let mut info = BasicInformation {
        exit_status: 0,
        peb: 0,
        affinity_mask: 0,
        base_priority: 0,
        pid: 0,
        parent: 0,
    };
    // SAFETY: class 0 (ProcessBasicInformation) writes exactly this structure's size into info.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            0,
            (&mut info as *mut BasicInformation).cast(),
            std::mem::size_of::<BasicInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 || info.peb == 0 {
        return None;
    }
    read_pointer(process, info.peb + layout::PARAMETERS).filter(|&address| address != 0)
}

/// The remote environment block as `NAME=value` entries.
fn environment(process: HANDLE) -> io::Result<Vec<String>> {
    let unreadable = || io::Error::other("process environment is unreadable");
    let params = parameters(process).ok_or_else(unreadable)?;
    let mut address = read_pointer(process, params + layout::ENVIRONMENT).ok_or_else(unreadable)?;
    if address == 0 {
        return Err(unreadable());
    }
    // Read page by page: the block's size field is absent before Windows 10, and a read
    // crossing into an uncommitted page fails as a whole.
    let mut units: Vec<u16> = Vec::new();
    let mut pending: Option<u8> = None;
    let mut scanned = 0;
    loop {
        let chunk = PAGE - address % PAGE;
        let mut bytes = vec![0u8; chunk];
        if !read_memory(process, address, &mut bytes) {
            return Err(unreadable());
        }
        address += chunk;
        let mut rest = bytes.as_slice();
        if let Some(low) = pending.take() {
            units.push(u16::from_ne_bytes([low, rest[0]]));
            rest = &rest[1..];
        }
        let (pairs, remainder) = rest.as_chunks::<2>();
        units.extend(pairs.iter().map(|&pair| u16::from_ne_bytes(pair)));
        pending = remainder.first().copied();
        if let Some(end) = units[scanned..].windows(2).position(|w| w == [0, 0]) {
            units.truncate(scanned + end);
            break;
        }
        scanned = units.len().saturating_sub(1);
        if units.len() * 2 > MAX_ENVIRONMENT_BYTES {
            return Err(unreadable());
        }
    }
    Ok(units
        .split(|&c| c == 0)
        .map(String::from_utf16_lossy)
        .collect())
}

fn working_set(process: HANDLE) -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: counters is writable and cb names its exact size.
    let ok = unsafe {
        GetProcessMemoryInfo(
            process,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    (ok != 0).then_some(counters.WorkingSetSize as u64)
}

fn executable(process: HANDLE) -> Option<String> {
    let mut buffer = vec![0u16; 32 * 1024];
    let mut size = buffer.len() as u32;
    // SAFETY: buffer holds size UTF-16 units; size is updated to the written length.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size)
    };
    (ok != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
}

/// What this user can learn about `pid`, or `None` when it cannot be opened at all.
pub fn details(pid: u32) -> Option<Details> {
    let pid = i32::try_from(pid).ok()?;
    let (handle, readable) = match win::open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ) {
        Some(handle) => (handle, true),
        None => (win::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?, false),
    };
    let (created, exited) = win::times(&handle)?;
    let raw = handle.as_raw_handle() as HANDLE;
    let params = readable.then(|| parameters(raw)).flatten();
    Some(Details {
        created,
        exited,
        working_set: working_set(raw),
        executable: executable(raw),
        command_line: params.and_then(|p| read_unicode_string(raw, p + layout::COMMAND_LINE)),
        cwd: params
            .and_then(|p| read_unicode_string(raw, p + layout::CURRENT_DIRECTORY))
            .map(|dir| match dir.strip_suffix('\\') {
                Some(trimmed) if !trimmed.ends_with(':') => trimmed.to_owned(),
                _ => dir,
            }),
    })
}

/// Terminate `pid` when it was created at or after `created_after` (a FILETIME) and its
/// environment holds exactly `entry` (`NAME=value`). Returns whether it was terminated. A process
/// this user cannot open, or that already exited, is not marked.
pub fn kill_marked(pid: u32, entry: &str, created_after: u64) -> io::Result<bool> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(false);
    };
    let access = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_TERMINATE;
    let Some(handle) = win::open(id, access) else {
        return Ok(false);
    };
    match win::times(&handle) {
        Some((created, false)) if created >= created_after => {}
        _ => return Ok(false),
    }
    let raw = handle.as_raw_handle() as HANDLE;
    let marked = match environment(raw) {
        Ok(environment) => environment.iter().any(|item| item == entry),
        // It may have exited between the checks; only a live unreadable process is an error.
        Err(_) if matches!(win::times(&handle), Some((_, true)) | None) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !marked {
        return Ok(false);
    }
    // SAFETY: handle pins the exact process whose identity and environment were checked above.
    if unsafe { TerminateProcess(raw, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Termination is asynchronous; wait briefly so a following snapshot sees it gone.
    // SAFETY: handle stays owned for this bounded wait.
    unsafe { WaitForSingleObject(raw, Duration::from_millis(100).as_millis() as u32) };
    Ok(true)
}

/// Fetch one IP Helper table, growing the buffer as the API asks. 8-byte backing keeps every
/// row aligned.
fn table(fetch: impl Fn(*mut c_void, &mut u32) -> u32) -> io::Result<Vec<u8>> {
    let mut size = 16 * 1024u32;
    for _ in 0..8 {
        let mut backing = vec![0u64; (size as usize).div_ceil(8)];
        let mut written = (backing.len() * 8) as u32;
        match fetch(backing.as_mut_ptr().cast(), &mut written) {
            NO_ERROR => {
                let bytes: Vec<u8> = backing.iter().flat_map(|word| word.to_ne_bytes()).collect();
                return Ok(bytes[..(written as usize).min(bytes.len())].to_vec());
            }
            ERROR_INSUFFICIENT_BUFFER => size = written.max(size) + 4096,
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
    Err(io::Error::other("socket table kept growing"))
}

/// Rows of a `{ dwNumEntries, table[] }` buffer.
fn rows<T: Copy>(bytes: &[u8]) -> Vec<T> {
    let Some(count) = bytes
        .get(..4)
        .map(|n| u32::from_ne_bytes([n[0], n[1], n[2], n[3]]))
    else {
        return Vec::new();
    };
    let size = std::mem::size_of::<T>();
    // Rows start after the count, at the row type's alignment.
    let start = 4usize.next_multiple_of(std::mem::align_of::<T>());
    (0..count as usize)
        .map_while(|index| {
            let offset = start + index * size;
            bytes.get(offset..offset + size).map(|row| {
                // SAFETY: row holds size_of::<T>() initialized bytes; T is a plain C row struct
                // of integers and byte arrays, valid for any bit pattern.
                unsafe { std::ptr::read_unaligned(row.as_ptr().cast::<T>()) }
            })
        })
        .collect()
}

fn port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// Listening TCP and bound UDP endpoints, IPv4 and IPv6.
pub fn listeners() -> io::Result<Vec<Listener>> {
    let mut out = Vec::new();
    let tcp = |family: u16| {
        table(|buffer, size| {
            // SAFETY: buffer has *size writable bytes; the API writes at most that many.
            unsafe {
                GetExtendedTcpTable(
                    buffer,
                    size,
                    0,
                    u32::from(family),
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            }
        })
    };
    let udp = |family: u16| {
        table(|buffer, size| {
            // SAFETY: as above.
            unsafe {
                GetExtendedUdpTable(buffer, size, 0, u32::from(family), UDP_TABLE_OWNER_PID, 0)
            }
        })
    };
    for row in rows::<MIB_TCPROW_OWNER_PID>(&tcp(AF_INET)?) {
        out.push(Listener {
            protocol: "tcp",
            address: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
            port: port(row.dwLocalPort),
            pid: row.dwOwningPid,
        });
    }
    for row in rows::<MIB_TCP6ROW_OWNER_PID>(&tcp(AF_INET6)?) {
        out.push(Listener {
            protocol: "tcp6",
            address: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
            port: port(row.dwLocalPort),
            pid: row.dwOwningPid,
        });
    }
    for row in rows::<MIB_UDPROW_OWNER_PID>(&udp(AF_INET)?) {
        out.push(Listener {
            protocol: "udp",
            address: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
            port: port(row.dwLocalPort),
            pid: row.dwOwningPid,
        });
    }
    for row in rows::<MIB_UDP6ROW_OWNER_PID>(&udp(AF_INET6)?) {
        out.push(Listener {
            protocol: "udp6",
            address: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
            port: port(row.dwLocalPort),
            pid: row.dwOwningPid,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
