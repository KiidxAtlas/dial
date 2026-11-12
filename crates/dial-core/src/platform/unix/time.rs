/// `(year, month, day, hour, minute, second)` in the local time zone.
pub(in crate::platform) fn local_fields(unix_ms: u64) -> Option<(i32, i32, i32, i32, i32, i32)> {
    let seconds = libc::time_t::try_from(unix_ms / 1000).ok()?;
    // SAFETY: `tm` is plain old data and is zero-initialised before `localtime_r` writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference live locals; a null return is checked below.
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    ))
}
