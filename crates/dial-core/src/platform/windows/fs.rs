//! Handle-based Windows filesystem confinement and private-file security.
use crate::platform::FileIdentity;
use std::{
    fs::{DirBuilder, File, Metadata, OpenOptions},
    io, mem,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path},
    ptr,
};
use windows_sys::{
    Wdk::{Foundation::OBJECT_ATTRIBUTES, Storage::FileSystem::NtCreateFile},
    Win32::{
        Foundation::{HANDLE, LocalFree, UNICODE_STRING},
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{
            IO::IO_STATUS_BLOCK,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    },
};
fn reject_reparse(file: &File) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // The query checks the opened object, not a path that another process can swap.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reparse points are not permitted",
        ));
    }
    Ok(())
}
pub(in crate::platform) fn open_nofollow(
    options: &OpenOptions,
    path: &Path,
    _: bool,
) -> io::Result<File> {
    let file = options
        .clone()
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    reject_reparse(&file)?;
    Ok(file)
}
pub(in crate::platform) fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    let file = open_nofollow(OpenOptions::new().read(true), path, false)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected directory",
        ));
    }
    Ok(file)
}
pub(in crate::platform) fn open_dir_nofollow_write(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | WRITE_DAC | DELETE | 0x40)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    reject_reparse(&file)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected directory",
        ));
    }
    Ok(file)
}
pub(in crate::platform) fn open_beneath(
    root: &Path,
    relative: &Path,
    directory: bool,
) -> io::Result<File> {
    let mut current = open_dir_nofollow(root)?;
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return if directory {
            Ok(current)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty file path",
            ))
        };
    }
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path must contain only plain names",
            ));
        };
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide.contains(&0)
            || wide.contains(&(b':' as u16))
            || wide.len() > (u16::MAX as usize / 2)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid path component",
            ));
        }
        let text = UNICODE_STRING {
            Length: (wide.len() * 2) as u16,
            MaximumLength: (wide.len() * 2) as u16,
            Buffer: wide.as_mut_ptr(),
        };
        let attrs = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: current.as_raw_handle() as HANDLE,
            ObjectName: &text,
            Attributes: 0x40,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        let mut handle = ptr::null_mut();
        let mut status = IO_STATUS_BLOCK::default();
        let is_dir = index + 1 != parts.len() || directory;
        // FILE_OPEN (1), FILE_SYNCHRONOUS_IO_NONALERT (0x20), FILE_OPEN_REPARSE_POINT (0x200000).
        let result = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_READ,
                &attrs,
                &mut status,
                ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                1,
                0x20 | 0x200000 | if is_dir { 1 } else { 0x40 },
                ptr::null(),
                0,
            )
        };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
            } as i32));
        }
        // NtCreateFile transferred one owned handle on success.
        let next = unsafe { File::from_raw_handle(handle.cast()) };
        reject_reparse(&next)?;
        current = next;
    }
    Ok(current)
}
fn relative_name(
    root: &File,
    relative: &Path,
    access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<File> {
    let mut current = root.try_clone()?;
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty relative path",
        ));
    }
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path must contain only plain names",
            ));
        };
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide.contains(&0)
            || wide.contains(&(b':' as u16))
            || wide.len() > u16::MAX as usize / 2
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid path component",
            ));
        }
        let mut wide = wide;
        let text = UNICODE_STRING {
            Length: (wide.len() * 2) as u16,
            MaximumLength: (wide.len() * 2) as u16,
            Buffer: wide.as_mut_ptr(),
        };
        let attrs = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: current.as_raw_handle() as HANDLE,
            ObjectName: &text,
            Attributes: 0x40,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        let mut handle = ptr::null_mut();
        let mut status = IO_STATUS_BLOCK::default();
        let last = index + 1 == parts.len();
        let result = unsafe {
            NtCreateFile(
                &mut handle,
                if last { access } else { FILE_GENERIC_READ },
                &attrs,
                &mut status,
                ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                if last { disposition } else { 1 },
                0x20 | 0x200000 | if last { options } else { 1 },
                ptr::null(),
                0,
            )
        };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
            } as i32));
        }
        let next = unsafe { File::from_raw_handle(handle.cast()) };
        reject_reparse(&next)?;
        current = next;
    }
    Ok(current)
}
pub(in crate::platform) fn create_new_beneath(
    root: &File,
    relative: &Path,
    owner_only: bool,
) -> io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "creation name must be one component",
        ));
    }
    let Component::Normal(name) = parts[0] else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid creation name",
        ));
    };
    let mut wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || wide.contains(&0)
        || wide.contains(&(b':' as u16))
        || wide.len() > u16::MAX as usize / 2
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid creation name",
        ));
    }
    let security = if owner_only {
        Some(private_security_descriptor()?)
    } else {
        None
    };
    let text = UNICODE_STRING {
        Length: (wide.len() * 2) as u16,
        MaximumLength: (wide.len() * 2) as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.as_raw_handle() as HANDLE,
        ObjectName: &text,
        Attributes: 0x40,
        SecurityDescriptor: security.as_ref().map_or(ptr::null(), |sd| {
            sd.0 as *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR
        }),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    let access = FILE_GENERIC_READ | FILE_GENERIC_WRITE | if owner_only { WRITE_DAC } else { 0 };
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attrs,
            &mut status,
            ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            2,
            0x20 | 0x200000 | 0x40,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(unsafe {
            windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
        } as i32));
    }
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    reject_reparse(&file)?;
    if owner_only {
        install_private_acl(&file)?;
    }
    Ok(file)
}
pub(in crate::platform) fn open_beneath_handle(root: &File, relative: &Path) -> io::Result<File> {
    relative_name(
        root,
        relative,
        FILE_GENERIC_READ | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | DELETE,
        1,
        0x40,
    )
}
pub(in crate::platform) fn open_dir_beneath_handle(
    root: &File,
    relative: &Path,
) -> io::Result<File> {
    let directory = relative_name(root, relative, FILE_GENERIC_READ, 1, 1)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a directory",
        ));
    }
    Ok(directory)
}
pub(in crate::platform) fn open_dir_beneath_handle_write(
    root: &File,
    relative: &Path,
) -> io::Result<File> {
    let access = FILE_GENERIC_READ | FILE_GENERIC_WRITE | WRITE_DAC | DELETE | 0x40;
    let directory = relative_name(root, relative, access, 1, 1)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a directory",
        ));
    }
    Ok(directory)
}
pub(in crate::platform) fn open_file_beneath(
    root: &File,
    relative: &Path,
    write: bool,
) -> io::Result<File> {
    let access = FILE_GENERIC_READ
        | if write {
            FILE_GENERIC_WRITE | WRITE_DAC
        } else {
            0
        };
    let file = relative_name(root, relative, access, 1, 0x40)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    Ok(file)
}
pub(in crate::platform) fn create_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    let dir = relative_name(root, relative, FILE_GENERIC_READ | WRITE_DAC, 2, 1)?;
    if let Err(error) = install_private_acl(&dir) {
        // Never leave a directory carrying the parent's inherited access behind.
        drop(dir);
        let _ = remove_dir_beneath(root, relative);
        return Err(error);
    }
    Ok(())
}
fn set_name(
    file: &File,
    root: &File,
    relative: &Path,
    class: i32,
    delete: bool,
    replace: bool,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let mut buffer;
    if delete {
        buffer = vec![1u8];
    } else {
        let parts: Vec<_> = relative.components().collect();
        if parts.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mutation name must be one component",
            ));
        }
        let Component::Normal(part) = parts[0] else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid mutation name",
            ));
        };
        let name: Vec<u16> = part.encode_wide().collect();
        if name.is_empty() || name.contains(&0) || name.contains(&(b':' as u16)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid mutation name",
            ));
        }
        buffer = vec![u8::from(replace); 20 + name.len() * 2];
        let root_handle = root.as_raw_handle() as usize;
        buffer[8..8 + mem::size_of::<usize>()].copy_from_slice(&root_handle.to_ne_bytes());
        buffer[16..20].copy_from_slice(&((name.len() * 2) as u32).to_ne_bytes());
        for (i, unit) in name.iter().enumerate() {
            buffer[20 + i * 2..22 + i * 2].copy_from_slice(&unit.to_ne_bytes());
        }
    }
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut status,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            class,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(unsafe {
            windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
        } as i32));
    }
    Ok(())
}
pub(in crate::platform) fn link_beneath(
    root: &File,
    existing: &Path,
    new: &Path,
) -> io::Result<()> {
    let file = open_beneath_handle(root, existing)?;
    set_name(&file, root, new, 11, false, false)
}
pub(in crate::platform) fn rename_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
    let file = open_beneath_handle(root, from)?;
    set_name(&file, root, to, 10, false, false)
}
pub(in crate::platform) fn replace_beneath(root: &File, from: &Path, to: &Path) -> io::Result<()> {
    let file = open_beneath_handle(root, from)?;
    set_name(&file, root, to, 10, false, true)
}
pub(in crate::platform) fn replace_beneath_file(
    root: &File,
    from: &Path,
    expected: &File,
    to: &Path,
) -> io::Result<()> {
    let source = open_beneath_handle(root, from)?;
    if identity(&source)? != identity(expected)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staged file changed before publication",
        ));
    }
    set_name(&source, root, to, 10, false, true)
}
pub(in crate::platform) fn remove_beneath(root: &File, relative: &Path) -> io::Result<()> {
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mutation name must be one component",
        ));
    }
    let Component::Normal(name) = parts[0] else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid mutation name",
        ));
    };
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || wide.contains(&0)
        || wide.contains(&(b':' as u16))
        || wide.len() > u16::MAX as usize / 2
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid mutation name",
        ));
    }
    let text = UNICODE_STRING {
        Length: (wide.len() * 2) as u16,
        MaximumLength: (wide.len() * 2) as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.as_raw_handle() as HANDLE,
        ObjectName: &text,
        Attributes: 0x40,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null_mut(),
    };
    let mut handle = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attrs,
            &mut status,
            ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            1,
            0x20 | 0x200000,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(unsafe {
            windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
        } as i32));
    }
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    set_name(&file, root, relative, 13, true, false)
}
pub(in crate::platform) fn remove_dir_beneath(root: &File, relative: &Path) -> io::Result<()> {
    let file = relative_name(
        root,
        relative,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        1,
        1,
    )?;
    set_name(&file, root, relative, 13, true, false)
}
pub(in crate::platform) fn read_dir_beneath(root: &File) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::windows::ffi::OsStringExt;
    let mut entries = Vec::new();
    let mut storage = vec![0usize; 32 * 1024 / mem::size_of::<usize>()];
    let mut restart = true;
    loop {
        let mut status = IO_STATUS_BLOCK::default();
        let result = unsafe {
            windows_sys::Wdk::Storage::FileSystem::NtQueryDirectoryFile(
                root.as_raw_handle() as HANDLE,
                ptr::null_mut(),
                None,
                ptr::null_mut(),
                &mut status,
                storage.as_mut_ptr().cast(),
                (storage.len() * mem::size_of::<usize>()) as u32,
                1,
                true,
                ptr::null_mut(),
                restart,
            )
        };
        restart = false;
        if result == 0x8000_0006u32 as i32 {
            break;
        }
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Foundation::RtlNtStatusToDosError(result)
            } as i32));
        }
        let bytes = storage.as_ptr().cast::<u8>();
        let length = unsafe { *(bytes.add(60).cast::<u32>()) } as usize;
        if length % 2 != 0 || length > storage.len() * mem::size_of::<usize>() - 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid directory entry returned by Windows",
            ));
        }
        let name = unsafe { std::slice::from_raw_parts(bytes.add(64).cast::<u16>(), length / 2) };
        if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
            entries.push(std::ffi::OsString::from_wide(name));
        }
    }
    Ok(entries)
}

struct Allocation(*mut std::ffi::c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}
fn current_user() -> io::Result<Vec<usize>> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(token.cast()) };
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut length,
        );
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0usize; (length as usize).div_ceil(mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}
fn user_sid(bytes: &[usize]) -> PSID {
    unsafe { (*(bytes.as_ptr().cast::<TOKEN_USER>())).User.Sid }
}
fn security(file: &File) -> io::Result<(Allocation, PSID, *mut ACL)> {
    let mut owner = ptr::null_mut();
    let mut acl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok((Allocation(descriptor), owner, acl))
}
pub(in crate::platform) fn owned_by_current_user(file: &File) -> io::Result<bool> {
    let (_sd, owner, _) = security(file)?;
    let token = current_user()?;
    Ok(!owner.is_null() && unsafe { EqualSid(owner, user_sid(&token)) } != 0)
}
pub(in crate::platform) fn path_owned_by_current_user(path: &Path) -> io::Result<bool> {
    owned_by_current_user(&open_nofollow(OpenOptions::new().read(true), path, false)?)
}
/// Owned by LocalSystem or BUILTIN\Administrators (system aliases such as junctions in the profile root).
pub(in crate::platform) fn path_owned_by_system(path: &Path) -> io::Result<bool> {
    let file = open_nofollow(OpenOptions::new().read(true), path, false)?;
    let (_sd, owner, _) = security(&file)?;
    Ok(!owner.is_null()
        && [WinLocalSystemSid, WinBuiltinAdministratorsSid]
            .into_iter()
            .any(|kind| unsafe { IsWellKnownSid(owner, kind) } != 0))
}
pub(in crate::platform) fn is_owner_only(file: &File) -> io::Result<bool> {
    let (_sd, owner, acl) = security(file)?;
    let token = current_user()?;
    let sid = user_sid(&token);
    // An elevated administrator token creates objects owned by BUILTIN\Administrators rather than by
    // its own user SID; the DACL below still grants access to the current user alone.
    let owned = !owner.is_null()
        && (unsafe { EqualSid(owner, sid) } != 0
            || unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0);
    if !owned || acl.is_null() {
        return Ok(false);
    }
    for index in 0..unsafe { (*acl).AceCount } as u32 {
        let mut raw = ptr::null_mut();
        if unsafe { GetAce(acl, index, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        match header.AceType {
            0 => {
                let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
                let principal = (&ace.SidStart as *const u32).cast_mut().cast();
                if unsafe { EqualSid(principal, sid) } == 0 {
                    return Ok(false);
                }
            }
            1 => {}                // Deny ACEs never widen access.
            _ => return Ok(false), // Unknown/callback/object ACEs require explicit inspection, fail closed.
        }
    }
    Ok(true)
}
fn private_security_descriptor() -> io::Result<Allocation> {
    let token = current_user()?;
    let sid = user_sid(&token);
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _text_owner = Allocation(text.cast());
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;{sid_text})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Allocation(descriptor))
}
fn install_private_acl(file: &File) -> io::Result<()> {
    let _owner = private_security_descriptor()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = ptr::null_mut();
    if unsafe { GetSecurityDescriptorDacl(_owner.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
    {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}
pub(in crate::platform) fn set_owner_only_file(file: &File) -> io::Result<()> {
    if !file.metadata()?.is_file() && !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner-only handle must be a file or directory",
        ));
    }
    install_private_acl(file)
}
pub(in crate::platform) fn set_owner_only(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    reject_reparse(&file)?;
    install_private_acl(&file)
}
fn file_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}
pub(in crate::platform) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file_info(file)?.nNumberOfLinks as u64)
}
pub(in crate::platform) fn identity(file: &File) -> io::Result<FileIdentity> {
    let info = file_info(file)?;
    Ok(FileIdentity {
        device: info.dwVolumeSerialNumber as u64,
        inode: (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64,
    })
}
pub(in crate::platform) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    identity(&open_nofollow(OpenOptions::new().read(true), path, false)?)
}
pub(in crate::platform) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    if mode & 0o077 == 0 {
        set_owner_only(path)?;
    }
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, permissions)
}
pub(in crate::platform) fn mode(meta: &Metadata) -> u32 {
    (if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }) | if meta.is_dir() { 0o111 } else { 0 }
}
pub(in crate::platform) fn is_executable(path: &Path) -> bool {
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.COM;.BAT;.CMD".into());
    path.is_file()
        && path.extension().is_some_and(|ext| {
            extensions.split(';').any(|candidate| {
                ext.to_string_lossy()
                    .eq_ignore_ascii_case(candidate.trim_start_matches('.'))
            })
        })
}
pub(in crate::platform) fn open_owner_only(options: &OpenOptions, path: &Path) -> io::Result<File> {
    // New children inherit protection from an already-private parent.
    if !path.exists() {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private file requires a parent",
            )
        })?;
        if !is_owner_only(&open_dir_nofollow(parent)?)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private creation requires a protected parent directory",
            ));
        }
    }
    let file = open_nofollow(options, path, false)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private file must be a regular file",
        ));
    }
    install_private_acl(&file)?;
    if !owned_by_current_user(&file)? || !is_owner_only(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file access cannot be enforced",
        ));
    }
    Ok(file)
}
pub(in crate::platform) fn create_dir_owner_only(
    builder: &DirBuilder,
    path: &Path,
) -> io::Result<()> {
    builder.create(path)?;
    if let Err(error) = set_owner_only(path) {
        // Never leave a directory carrying the parent's inherited access behind.
        let _ = std::fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}
#[cfg(test)]
mod transaction_tests;
