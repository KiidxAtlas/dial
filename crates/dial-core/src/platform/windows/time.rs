pub(in crate::platform) fn local_strftime(fmt: &str) -> String {
    use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
    let mut t = SYSTEMTIME::default();
    // SAFETY: t is writable, correctly aligned SYSTEMTIME storage for this synchronous query.
    unsafe { GetLocalTime(&mut t) }
    let mut out = String::new();
    let mut i = fmt.chars();
    while let Some(c) = i.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match i.next() {
            Some('Y') => out.push_str(&format!("{:04}", t.wYear)),
            Some('m') => out.push_str(&format!("{:02}", t.wMonth)),
            Some('d') => out.push_str(&format!("{:02}", t.wDay)),
            Some('H') => out.push_str(&format!("{:02}", t.wHour)),
            Some('M') => out.push_str(&format!("{:02}", t.wMinute)),
            Some('S') => out.push_str(&format!("{:02}", t.wSecond)),
            Some('%') => out.push('%'),
            _ => return String::new(),
        }
    }
    out
}

/// `(year, month, day, hour, minute, second)` in the local time zone.
pub(in crate::platform) fn local_fields(unix_ms: u64) -> Option<(i32, i32, i32, i32, i32, i32)> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    };
    /// FILETIME counts 100-ns ticks from 1601-01-01 UTC; this is the offset to the Unix epoch.
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    let ticks = unix_ms
        .checked_mul(10_000)?
        .checked_add(UNIX_EPOCH_AS_FILETIME)?;
    let utc_filetime = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: both pointers reference live locals of the Win32 types the APIs expect; a null
    // zone selects the current time zone. Zero return means failure and is checked below.
    unsafe {
        if FileTimeToSystemTime(&utc_filetime, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            return None;
        }
    }
    Some((
        local.wYear.into(),
        local.wMonth.into(),
        local.wDay.into(),
        local.wHour.into(),
        local.wMinute.into(),
        local.wSecond.into(),
    ))
}
