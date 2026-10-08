use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, LocalFree},
    Security::{self, Authorization::*, *},
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn user() -> io::Result<Vec<u8>> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Token(token);
    let mut size = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size);
    }
    let mut bytes = vec![0; size as usize];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}
fn user_sid(bytes: &[u8]) -> PSID {
    unsafe {
        ptr::read_unaligned(bytes.as_ptr().cast::<TOKEN_USER>())
            .User
            .Sid
    }
}
pub(super) fn owned(file: &File) -> io::Result<bool> {
    let user = user()?;
    let mut owner = ptr::null_mut();
    let mut sd = ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _sd = Descriptor(sd);
    Ok(unsafe { EqualSid(owner, user_sid(&user)) } != 0)
}
pub(super) fn single_link(file: &File) -> io::Result<bool> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}
pub(super) fn open_security(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}
pub(super) fn seal(file: &File) -> io::Result<()> {
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let reopened = unsafe { File::from_raw_handle(handle) };
    let file = &reopened;
    if super::is_alias(&file.metadata()?) || !owned(file)? {
        return Err(io::Error::other(
            "private storage must be owned by the current user and cannot be an alias",
        ));
    }
    if file.metadata()?.is_file() && !single_link(file)? {
        return Err(io::Error::other("private file cannot have hard links"));
    }
    let user = user()?;
    let mut sid_string = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user_sid(&user), &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut count = 0;
    unsafe {
        while *sid_string.add(count) != 0 {
            count += 1;
        }
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_string, count) });
    unsafe {
        LocalFree(sid_string.cast());
    }
    // Protect the DACL from inherited broad permissions. Children inherit only
    // the current user's and SYSTEM's access; no administrator privileges needed.
    let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut sd = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sd = Descriptor(sd);
    let mut dacl = ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(io::Error::other("invalid private DACL"));
    }
    let error = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}
