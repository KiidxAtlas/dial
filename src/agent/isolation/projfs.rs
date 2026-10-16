//! Windows Projected File System (ProjFS) isolation.
//!
//! The project is projected read-only into an empty instance directory: the worker sees the
//! source tree, file contents are served on demand from the source, and only the paths the worker
//! writes become real files in the instance directory. The source itself is never written.
//!
//! The change set comes from two places: the notifications ProjFS delivers for writes, creates,
//! renames and deletes (the journal), and a walk of the instance directory. Untouched placeholders
//! keep the stamp they had in the source, so they are never read or hashed.
//!
//! ProjFS is reached through `ProjectedFSLib.dll`, loaded at runtime. Linking it statically would
//! stop the binary from starting on Windows installs without the optional `Client-ProjFS` feature.
//! The platform calls are compile-checked for Windows only; the change-set rules are plain Rust and
//! are tested on every platform.

use super::{Snapshot, stamp};
use crate::{agent::agent_defs::TaskConfig, workspace_copy::is_excluded_source_path};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

/// Paths the worker wrote (`touched`) or deleted (`removed`), relative to the instance directory.
#[derive(Clone, Default)]
struct Changes {
    touched: BTreeSet<PathBuf>,
    removed: BTreeSet<PathBuf>,
}

/// Notification journal shared between ProjFS callbacks and the merge.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Journal {
    state: Mutex<Changes>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Journal {
    fn touch(&self, path: PathBuf) {
        let mut changes = self.lock();
        changes.removed.remove(&path);
        changes.touched.insert(path);
    }

    fn remove(&self, path: PathBuf) {
        let mut changes = self.lock();
        changes
            .touched
            .retain(|touched| !touched.starts_with(&path));
        changes.removed.insert(path);
    }

    /// True when `path` or one of its ancestors was deleted or renamed away.
    fn is_removed(&self, path: &Path) -> bool {
        covers(&self.lock().removed, path)
    }

    fn changes(&self) -> Changes {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Changes> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A live projection of the source into the instance directory. Dropping it stops
/// virtualization; the caller removes the directory afterwards.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct Projection {
    root: PathBuf,
    journal: Arc<Journal>,
    #[cfg(windows)]
    _instance: native::Instance,
}

impl Projection {
    /// The worker's changes as a snapshot comparable with `before`, the source snapshot.
    pub(super) fn changed_snapshot(
        &self,
        before: &Snapshot,
        limits: &TaskConfig,
    ) -> Result<Snapshot> {
        change_set(&self.root, &self.journal.changes(), before, limits)
    }
}

/// Whether ProjFS can be used on this machine. Loads `ProjectedFSLib.dll` and its entry points.
#[cfg(windows)]
pub(super) fn probe() -> Result<(), String> {
    native::probe()
}

/// Project `source` read-only into the empty directory `root`, which this creates.
#[cfg(windows)]
pub(super) fn start(source: &Path, root: &Path) -> Result<Projection, String> {
    let journal = Arc::new(Journal::default());
    let instance = native::start(source, root, Arc::clone(&journal))?;
    Ok(Projection {
        root: root.to_path_buf(),
        journal,
        _instance: instance,
    })
}

#[cfg(not(windows))]
pub(super) fn start(_source: &Path, _root: &Path) -> Result<Projection, String> {
    Err(UNSUPPORTED.into())
}

#[cfg(not(windows))]
const UNSUPPORTED: &str = "ProjFS isolation is only available on Windows";

/// True when `path` or one of its ancestors is in `set`. Path comparison is by whole component.
fn covers(set: &BTreeSet<PathBuf>, path: &Path) -> bool {
    path.ancestors().any(|ancestor| set.contains(ancestor))
}

/// Applies the journal to `before`: removed paths drop out, and touched or new files of the
/// instance directory are stamped with their current content. Every other file keeps its source
/// stamp, so a placeholder the worker only read is never read again.
fn change_set(
    root: &Path,
    changes: &Changes,
    before: &Snapshot,
    limits: &TaskConfig,
) -> Result<Snapshot> {
    let mut after = before.clone();
    after.retain(|path, _| !covers(&changes.removed, path));
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            visited += 1;
            ensure!(
                visited <= limits.isolation_source_files,
                "isolated copy has more than {} changed entries",
                limits.isolation_source_files
            );
            let path = entry.path();
            if is_excluded_source_path(root, &path) {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .context("ProjFS instance entry outside the instance directory")?
                .to_path_buf();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && (changes.touched.contains(&relative) || !before.contains_key(&relative))
            {
                match stamp(&path)? {
                    Some(stamp) => {
                        after.insert(relative, stamp);
                    }
                    None => {
                        after.remove(&relative);
                    }
                }
            }
        }
    }
    Ok(after)
}

#[cfg(windows)]
mod native {
    use super::Journal;
    use crate::workspace_copy::is_excluded_source_path;
    use std::{
        ffi::{OsStr, OsString, c_void},
        fs::{self, File},
        io::{self, Read, Seek, SeekFrom},
        mem::size_of,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        ptr,
        sync::{Arc, LazyLock, Mutex, PoisonError},
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows_sys::{
        Win32::{
            Foundation::{FARPROC, HMODULE},
            Storage::{
                FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
                ProjectedFileSystem::{
                    PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
                    PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
                    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_NOTIFICATION,
                    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
                    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
                    PRJ_NOTIFICATION_FILE_OVERWRITTEN, PRJ_NOTIFICATION_FILE_RENAMED,
                    PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFICATION_NEW_FILE_CREATED,
                    PRJ_NOTIFICATION_PARAMETERS, PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED,
                    PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED, PRJ_NOTIFY_FILE_OVERWRITTEN,
                    PRJ_NOTIFY_FILE_RENAMED, PRJ_NOTIFY_NEW_FILE_CREATED, PRJ_PLACEHOLDER_INFO,
                    PRJ_PLACEHOLDER_VERSION_INFO, PRJ_STARTVIRTUALIZING_OPTIONS,
                },
            },
            System::{
                Com::CoCreateGuid,
                LibraryLoader::{GetProcAddress, LoadLibraryW},
            },
        },
        core::{GUID, HRESULT, PCSTR, PCWSTR},
    };

    const S_OK: HRESULT = 0;
    const E_FAIL: HRESULT = 0x8000_4005u32 as i32;
    const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
    const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as i32;
    const E_FILE_NOT_FOUND: HRESULT = 0x8007_0002u32 as i32;

    /// Callbacks ProjFS may run at once; bounded so a busy worker cannot exhaust threads.
    const CONCURRENT_CALLBACKS: u32 = 4;
    const POOL_THREADS: u32 = 4;

    const NOTIFY_MASK: u32 = PRJ_NOTIFY_NEW_FILE_CREATED
        | PRJ_NOTIFY_FILE_OVERWRITTEN
        | PRJ_NOTIFY_FILE_RENAMED
        | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
        | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED;

    const UNAVAILABLE: &str = "ProjFS needs the optional Windows feature Client-ProjFS: enable it under Turn Windows features on or off, restart Windows, then retry (auto selection falls back to another backend)";

    type MarkFn = unsafe extern "system" fn(
        PCWSTR,
        PCWSTR,
        *const PRJ_PLACEHOLDER_VERSION_INFO,
        *const GUID,
    ) -> HRESULT;
    type StartFn = unsafe extern "system" fn(
        PCWSTR,
        *const PRJ_CALLBACKS,
        *const c_void,
        *const PRJ_STARTVIRTUALIZING_OPTIONS,
        *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    ) -> HRESULT;
    type StopFn = unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
    type FillFn = unsafe extern "system" fn(
        PCWSTR,
        *const PRJ_FILE_BASIC_INFO,
        PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT;
    type PlaceholderFn = unsafe extern "system" fn(
        PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        PCWSTR,
        *const PRJ_PLACEHOLDER_INFO,
        u32,
    ) -> HRESULT;
    type WriteDataFn = unsafe extern "system" fn(
        PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        *const GUID,
        *const c_void,
        u64,
        u32,
    ) -> HRESULT;
    type AllocFn =
        unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, usize) -> *mut c_void;
    type FreeFn = unsafe extern "system" fn(*const c_void);
    type MatchFn = unsafe extern "system" fn(PCWSTR, PCWSTR) -> bool;

    /// ProjFS entry points, resolved from `ProjectedFSLib.dll` at runtime.
    #[derive(Clone, Copy)]
    struct Api {
        mark: MarkFn,
        start: StartFn,
        stop: StopFn,
        fill: FillFn,
        placeholder: PlaceholderFn,
        write_data: WriteDataFn,
        alloc: AllocFn,
        free: FreeFn,
        matches: MatchFn,
    }

    static API: LazyLock<Result<Api, String>> = LazyLock::new(load);

    fn api() -> Result<Api, String> {
        API.clone()
    }

    fn load() -> Result<Api, String> {
        let name = wide(OsStr::new("ProjectedFSLib.dll"));
        // SAFETY: `name` is NUL-terminated UTF-16 and outlives the call.
        let module: HMODULE = unsafe { LoadLibraryW(name.as_ptr()) };
        if module.is_null() {
            return Err(format!("{UNAVAILABLE} ({})", io::Error::last_os_error()));
        }
        Ok(Api {
            mark: symbol(module, b"PrjMarkDirectoryAsPlaceholder\0")?,
            start: symbol(module, b"PrjStartVirtualizing\0")?,
            stop: symbol(module, b"PrjStopVirtualizing\0")?,
            fill: symbol(module, b"PrjFillDirEntryBuffer\0")?,
            placeholder: symbol(module, b"PrjWritePlaceholderInfo\0")?,
            write_data: symbol(module, b"PrjWriteFileData\0")?,
            alloc: symbol(module, b"PrjAllocateAlignedBuffer\0")?,
            free: symbol(module, b"PrjFreeAlignedBuffer\0")?,
            matches: symbol(module, b"PrjFileNameMatch\0")?,
        })
    }

    /// Resolves one export. `name` is NUL-terminated; `T` must be the matching function pointer type.
    fn symbol<T>(module: HMODULE, name: &[u8]) -> Result<T, String> {
        // SAFETY: `name` is NUL-terminated and `module` is a loaded module handle.
        let proc: FARPROC = unsafe { GetProcAddress(module, name.as_ptr() as PCSTR) };
        let Some(proc) = proc else {
            return Err(format!(
                "{UNAVAILABLE}: ProjectedFSLib.dll has no export {}",
                String::from_utf8_lossy(&name[..name.len() - 1])
            ));
        };
        // SAFETY: `T` is a function pointer type with the same size as the pointer returned by
        // GetProcAddress, and the signature matches the documented ProjFS export.
        Ok(unsafe { std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, T>(&proc) })
    }

    pub(super) fn probe() -> Result<(), String> {
        api().map(|_| ())
    }

    /// Callback state reached through `InstanceContext`.
    struct Context {
        source: PathBuf,
        journal: Arc<Journal>,
        sessions: Mutex<Vec<Session>>,
        api: Api,
    }

    /// One directory enumeration in progress. ProjFS may ask for its entries in several calls.
    struct Session {
        id: GUID,
        entries: Vec<Entry>,
        next: usize,
        pattern: Option<Vec<u16>>,
    }

    /// A directory entry of the source, with the name NUL-terminated for ProjFS.
    struct Entry {
        name: Vec<u16>,
        is_dir: bool,
        size: i64,
        time: i64,
    }

    /// A running projection. Dropping it stops virtualization before the context is released.
    pub(super) struct Instance {
        api: Api,
        handle: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
        _context: Arc<Context>,
    }

    // SAFETY: the handle is an opaque ProjFS token that is only used to stop the projection in
    // Drop, and ProjFS itself allows the projection to be stopped from any thread.
    unsafe impl Send for Instance {}

    impl Drop for Instance {
        fn drop(&mut self) {
            // SAFETY: `handle` came from PrjStartVirtualizing and is stopped exactly once, here.
            unsafe { (self.api.stop)(self.handle) };
        }
    }

    pub(super) fn start(
        source: &Path,
        root: &Path,
        journal: Arc<Journal>,
    ) -> Result<Instance, String> {
        let api = api()?;
        fs::create_dir(root).map_err(|error| format!("creating {}: {error}", root.display()))?;
        let mut instance_id = GUID::default();
        // SAFETY: `instance_id` is a valid out-pointer.
        check(
            unsafe { CoCreateGuid(&mut instance_id) },
            "creating the ProjFS instance id",
        )?;
        let root_w = wide(root.as_os_str());
        // SAFETY: a NULL target and version info mark the root itself; the pointers are valid for
        // the call.
        check(
            unsafe { (api.mark)(root_w.as_ptr(), ptr::null(), ptr::null(), &instance_id) },
            "marking the instance directory for ProjFS",
        )?;
        let context = Arc::new(Context {
            source: source.to_path_buf(),
            journal,
            sessions: Mutex::new(Vec::new()),
            api,
        });
        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(start_enumeration),
            EndDirectoryEnumerationCallback: Some(end_enumeration),
            GetDirectoryEnumerationCallback: Some(get_enumeration),
            GetPlaceholderInfoCallback: Some(placeholder_info),
            GetFileDataCallback: Some(file_data),
            QueryFileNameCallback: None,
            NotificationCallback: Some(notification),
            CancelCommandCallback: None,
        };
        // The empty root pattern maps the whole instance directory.
        let root_pattern = [0u16];
        let mut mapping = PRJ_NOTIFICATION_MAPPING {
            NotificationBitMask: NOTIFY_MASK,
            NotificationRoot: root_pattern.as_ptr(),
        };
        let options = PRJ_STARTVIRTUALIZING_OPTIONS {
            Flags: 0,
            PoolThreadCount: POOL_THREADS,
            ConcurrentThreadCount: CONCURRENT_CALLBACKS,
            NotificationMappings: &mut mapping,
            NotificationMappingsCount: 1,
        };
        let mut handle: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT = ptr::null_mut();
        // SAFETY: every pointer outlives the call. `context` lives in the returned Instance until
        // PrjStopVirtualizing has returned, so the callbacks never see a dangling instance context.
        check(
            unsafe {
                (api.start)(
                    root_w.as_ptr(),
                    &callbacks,
                    Arc::as_ptr(&context).cast(),
                    &options,
                    &mut handle,
                )
            },
            "starting ProjFS virtualization",
        )?;
        Ok(Instance {
            api,
            handle,
            _context: context,
        })
    }

    fn check(value: HRESULT, what: &str) -> Result<(), String> {
        if value >= 0 {
            Ok(())
        } else {
            Err(format!("{what} failed (HRESULT 0x{:08X})", value as u32))
        }
    }

    fn context<'a>(data: *const PRJ_CALLBACK_DATA) -> &'a Context {
        // SAFETY: ProjFS passes the InstanceContext given to PrjStartVirtualizing, which the
        // Instance keeps alive until virtualization stops.
        unsafe { &*((*data).InstanceContext as *const Context) }
    }

    /// Copies a NUL-terminated UTF-16 string, including the terminator.
    fn copy_wide(ptr: PCWSTR) -> Vec<u16> {
        if ptr.is_null() {
            return vec![0];
        }
        // SAFETY: ProjFS passes NUL-terminated strings that stay valid for the callback.
        unsafe {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(ptr, len + 1).to_vec()
        }
    }

    /// The relative path ProjFS reports; the root is the empty path.
    fn relative(ptr: PCWSTR) -> PathBuf {
        let units = copy_wide(ptr);
        PathBuf::from(OsString::from_wide(&units[..units.len() - 1]))
    }

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Windows FILETIME: 100-ns intervals since 1601-01-01.
    fn filetime(time: SystemTime) -> i64 {
        let since_unix = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        (since_unix.as_nanos() / 100) as i64 + 116_444_736_000_000_000
    }

    fn basic_info(is_dir: bool, size: i64, time: i64) -> PRJ_FILE_BASIC_INFO {
        PRJ_FILE_BASIC_INFO {
            IsDirectory: is_dir,
            FileSize: if is_dir { 0 } else { size },
            CreationTime: time,
            LastAccessTime: time,
            LastWriteTime: time,
            ChangeTime: time,
            FileAttributes: if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
        }
    }

    /// Maps an I/O error to an HRESULT, using the Win32 code when there is one.
    fn hresult_of(error: &io::Error) -> HRESULT {
        match error.raw_os_error() {
            Some(code) if code > 0 => ((code as u32 & 0xFFFF) | 0x8007_0000) as i32,
            _ => E_FAIL,
        }
    }

    fn same_guid(left: &GUID, right: &GUID) -> bool {
        left.data1 == right.data1
            && left.data2 == right.data2
            && left.data3 == right.data3
            && left.data4 == right.data4
    }

    /// The visible entries of the source directory `relative`, sorted by name. Excluded and
    /// deleted paths are left out, so they are invisible to the worker.
    fn list(source: &Path, relative: &Path, journal: &Journal) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(source.join(relative))? {
            let entry = entry?;
            let path = entry.path();
            if is_excluded_source_path(source, &path)
                || journal.is_removed(&relative.join(entry.file_name()))
            {
                continue;
            }
            let meta = entry.metadata()?;
            entries.push(Entry {
                name: wide(&entry.file_name()),
                is_dir: meta.is_dir(),
                size: meta.len() as i64,
                time: filetime(meta.modified()?),
            });
        }
        entries.sort_by_cached_key(|entry| {
            String::from_utf16_lossy(&entry.name[..entry.name.len() - 1]).to_uppercase()
        });
        Ok(entries)
    }

    extern "system" fn start_enumeration(
        data: *const PRJ_CALLBACK_DATA,
        id: *const GUID,
    ) -> HRESULT {
        let context = context(data);
        // SAFETY: ProjFS passes a valid enumeration id for the call.
        let id = unsafe { *id };
        // SAFETY: `data` is valid for the callback.
        let directory = relative(unsafe { (*data).FilePathName });
        match list(&context.source, &directory, &context.journal) {
            Ok(entries) => {
                context
                    .sessions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Session {
                        id,
                        entries,
                        next: 0,
                        pattern: None,
                    });
                S_OK
            }
            Err(error) => hresult_of(&error),
        }
    }

    extern "system" fn end_enumeration(data: *const PRJ_CALLBACK_DATA, id: *const GUID) -> HRESULT {
        let context = context(data);
        // SAFETY: ProjFS passes a valid enumeration id for the call.
        let id = unsafe { *id };
        context
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|session| !same_guid(&session.id, &id));
        S_OK
    }

    extern "system" fn get_enumeration(
        data: *const PRJ_CALLBACK_DATA,
        id: *const GUID,
        search: PCWSTR,
        buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT {
        let context = context(data);
        // SAFETY: ProjFS passes a valid enumeration id and callback data for the call.
        let (id, restart) =
            unsafe { (*id, (*data).Flags & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN != 0) };
        let mut sessions = context
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(session) = sessions
            .iter_mut()
            .find(|session| same_guid(&session.id, &id))
        else {
            return E_INVALIDARG;
        };
        if restart {
            session.next = 0;
        }
        if !search.is_null() {
            session.pattern = Some(copy_wide(search));
        }
        while session.next < session.entries.len() {
            let entry = &session.entries[session.next];
            let matched = session.pattern.as_ref().is_none_or(|pattern| {
                // SAFETY: both names are NUL-terminated and valid for the call.
                unsafe { (context.api.matches)(entry.name.as_ptr(), pattern.as_ptr()) }
            });
            if matched {
                let info = basic_info(entry.is_dir, entry.size, entry.time);
                // SAFETY: `name` and `info` are valid for the call; `buffer` is the handle ProjFS
                // gave this callback. A negative result (buffer full) is returned so ProjFS calls
                // again, and the entry at `next` is offered first.
                let hr = unsafe { (context.api.fill)(entry.name.as_ptr(), &info, buffer) };
                if hr < 0 {
                    return hr;
                }
            }
            session.next += 1;
        }
        S_OK
    }

    extern "system" fn placeholder_info(data: *const PRJ_CALLBACK_DATA) -> HRESULT {
        let context = context(data);
        // SAFETY: `data` is valid for the callback.
        let (ns, name) = unsafe { ((*data).NamespaceVirtualizationContext, (*data).FilePathName) };
        let rel = relative(name);
        let source = context.source.join(&rel);
        if context.journal.is_removed(&rel) || is_excluded_source_path(&context.source, &source) {
            return E_FILE_NOT_FOUND;
        }
        let meta = match fs::metadata(&source) {
            Ok(meta) => meta,
            Err(error) => return hresult_of(&error),
        };
        let time = meta.modified().map(filetime).unwrap_or(0);
        // SAFETY: a zeroed PRJ_PLACEHOLDER_INFO is valid (plain integers, GUIDs and a byte array);
        // its basic info is set below and the version info is left empty, as ProjFS allows.
        let mut info: PRJ_PLACEHOLDER_INFO = unsafe { std::mem::zeroed() };
        info.FileBasicInfo = basic_info(meta.is_dir(), meta.len() as i64, time);
        // SAFETY: `info` is valid for the call and its size is passed exactly.
        unsafe {
            (context.api.placeholder)(ns, name, &info, size_of::<PRJ_PLACEHOLDER_INFO>() as u32)
        }
    }

    extern "system" fn file_data(
        data: *const PRJ_CALLBACK_DATA,
        offset: u64,
        length: u32,
    ) -> HRESULT {
        let context = context(data);
        if length == 0 {
            return S_OK;
        }
        // SAFETY: `data` is valid for the callback.
        let (ns, stream, name) = unsafe {
            (
                (*data).NamespaceVirtualizationContext,
                (*data).DataStreamId,
                (*data).FilePathName,
            )
        };
        let rel = relative(name);
        if context.journal.is_removed(&rel) {
            return E_FILE_NOT_FOUND;
        }
        let source = context.source.join(&rel);
        let length_usize = length as usize;
        // SAFETY: the namespace context comes from ProjFS for this callback.
        let buffer = unsafe { (context.api.alloc)(ns, length_usize) };
        if buffer.is_null() {
            return E_OUTOFMEMORY;
        }
        // SAFETY: `buffer` is a fresh allocation of `length_usize` bytes, used only until it is freed.
        let bytes = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length_usize) };
        let read = File::open(&source).and_then(|mut file| {
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(bytes)
        });
        let hr = match read {
            // SAFETY: the buffer holds exactly `length` bytes read from the source.
            Ok(()) => unsafe { (context.api.write_data)(ns, &stream, buffer, offset, length) },
            Err(error) => hresult_of(&error),
        };
        // SAFETY: `buffer` came from PrjAllocateAlignedBuffer and is not used after this call.
        unsafe { (context.api.free)(buffer) };
        hr
    }

    extern "system" fn notification(
        data: *const PRJ_CALLBACK_DATA,
        _is_directory: bool,
        kind: PRJ_NOTIFICATION,
        destination: PCWSTR,
        _parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
    ) -> HRESULT {
        let context = context(data);
        // SAFETY: `data` is valid for the callback.
        let rel = relative(unsafe { (*data).FilePathName });
        match kind {
            PRJ_NOTIFICATION_NEW_FILE_CREATED
            | PRJ_NOTIFICATION_FILE_OVERWRITTEN
            | PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => context.journal.touch(rel),
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED => context.journal.remove(rel),
            PRJ_NOTIFICATION_FILE_RENAMED => {
                context.journal.remove(rel);
                if !destination.is_null() {
                    context.journal.touch(relative(destination));
                }
            }
            _ => {}
        }
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn limits(files: usize) -> TaskConfig {
        TaskConfig {
            isolation_source_files: files,
            ..TaskConfig::default()
        }
    }

    /// Stamps the named files under `dir`, as the source snapshot `before`.
    fn source_snapshot(dir: &Path, names: &[&str]) -> Snapshot {
        names
            .iter()
            .map(|name| {
                (
                    PathBuf::from(name),
                    stamp(&dir.join(name)).unwrap().unwrap(),
                )
            })
            .collect()
    }

    fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn removing_a_path_hides_everything_below_it() {
        let journal = Journal::default();
        journal.remove(PathBuf::from("dir"));
        assert!(journal.is_removed(Path::new("dir/a/b.txt")));
        assert!(journal.is_removed(Path::new("dir")));
        assert!(!journal.is_removed(Path::new("directory.txt")));
        assert!(!journal.is_removed(Path::new("other.txt")));
    }

    #[test]
    fn recreating_a_path_clears_its_tombstone_and_marks_it_touched() {
        let journal = Journal::default();
        journal.remove(PathBuf::from("a.txt"));
        journal.touch(PathBuf::from("a.txt"));
        assert!(!journal.is_removed(Path::new("a.txt")));
        assert_eq!(journal.changes().touched, paths(&["a.txt"]));
    }

    #[test]
    fn removing_a_directory_forgets_touches_below_it() {
        let journal = Journal::default();
        journal.touch(PathBuf::from("dir/new.txt"));
        journal.remove(PathBuf::from("dir"));
        assert!(journal.changes().touched.is_empty());
    }

    #[test]
    fn untouched_placeholders_keep_their_source_stamp_without_being_reread() {
        let source = tempfile::tempdir().unwrap();
        let instance = tempfile::tempdir().unwrap();
        fs::write(source.path().join("a.txt"), "source").unwrap();
        fs::write(instance.path().join("a.txt"), "other bytes entirely").unwrap();
        let before = source_snapshot(source.path(), &["a.txt"]);
        let after =
            change_set(instance.path(), &Changes::default(), &before, &limits(100)).unwrap();
        assert_eq!(
            after[Path::new("a.txt")].hash,
            before[Path::new("a.txt")].hash
        );
    }

    #[test]
    fn touched_files_are_restamped_from_the_instance_directory() {
        let source = tempfile::tempdir().unwrap();
        let instance = tempfile::tempdir().unwrap();
        fs::write(source.path().join("a.txt"), "source").unwrap();
        fs::write(instance.path().join("a.txt"), "worker edit").unwrap();
        let before = source_snapshot(source.path(), &["a.txt"]);
        let changes = Changes {
            touched: paths(&["a.txt"]),
            ..Changes::default()
        };
        let after = change_set(instance.path(), &changes, &before, &limits(100)).unwrap();
        let expected = stamp(&instance.path().join("a.txt")).unwrap().unwrap();
        assert_eq!(after[Path::new("a.txt")].hash, expected.hash);
        assert_ne!(
            after[Path::new("a.txt")].hash,
            before[Path::new("a.txt")].hash
        );
    }

    #[test]
    fn new_files_are_added_and_excluded_paths_never_are() {
        let instance = tempfile::tempdir().unwrap();
        fs::write(instance.path().join("new.txt"), "new").unwrap();
        fs::write(instance.path().join(".env"), "SECRET=1").unwrap();
        let after = change_set(
            instance.path(),
            &Changes::default(),
            &Snapshot::new(),
            &limits(100),
        )
        .unwrap();
        assert!(after.contains_key(Path::new("new.txt")));
        assert!(!after.contains_key(Path::new(".env")));
    }

    #[test]
    fn removed_files_and_directories_leave_the_change_set_and_others_stay() {
        let source = tempfile::tempdir().unwrap();
        let instance = tempfile::tempdir().unwrap();
        fs::write(source.path().join("a.txt"), "a").unwrap();
        fs::write(source.path().join("keep.txt"), "k").unwrap();
        fs::create_dir(source.path().join("dir")).unwrap();
        fs::write(source.path().join("dir").join("b.txt"), "b").unwrap();
        let before = source_snapshot(source.path(), &["a.txt", "keep.txt", "dir/b.txt"]);
        let changes = Changes {
            removed: paths(&["a.txt", "dir"]),
            ..Changes::default()
        };
        let after = change_set(instance.path(), &changes, &before, &limits(100)).unwrap();
        assert_eq!(
            after.keys().cloned().collect::<BTreeSet<_>>(),
            paths(&["keep.txt"])
        );
    }

    #[test]
    fn the_change_set_respects_the_entry_budget() {
        let instance = tempfile::tempdir().unwrap();
        fs::write(instance.path().join("one.txt"), "1").unwrap();
        fs::write(instance.path().join("two.txt"), "2").unwrap();
        let error = change_set(
            instance.path(),
            &Changes::default(),
            &Snapshot::new(),
            &limits(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("more than 1 changed entries"));
    }
}
