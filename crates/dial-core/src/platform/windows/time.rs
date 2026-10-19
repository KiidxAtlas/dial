pub(in crate::platform) fn local_strftime(fmt: &str) -> String {
    use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
    let mut t = SYSTEMTIME::default();
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
