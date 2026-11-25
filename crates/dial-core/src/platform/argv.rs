//! Keep free-text arguments out of the process list.
//!
//! On Linux a process's command line (`/proc/<pid>/cmdline`, which `ps`, `pgrep -f` and `pkill -f`
//! match) is read from the process's own argument memory. A prompt passed as an argument therefore
//! makes the agent's process match commands it runs for the user: `pkill -f qemu`, or a loop that
//! kills every process whose command line mentions qemu, kills the agent itself when the prompt
//! mentions qemu. Overwriting those bytes in place blanks them for every later reader, including
//! `std::env::args_os`. The write goes through `/proc/self/mem`, so no raw pointer is involved.

use std::io;

/// Replaces with spaces every argument byte-equal to one of `texts` and returns how many were
/// blanked. On other platforms the command line is left unchanged and 0 is returned.
pub fn blank_arguments(texts: &[&str]) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let stat = std::fs::read_to_string("/proc/self/stat")?;
        let (start, end) = argument_range(&stat)
            .ok_or_else(|| io::Error::other("/proc/self/stat has no argument range"))?;
        let length = usize::try_from(end.saturating_sub(start))
            .map_err(|_| io::Error::other("argument range does not fit in memory"))?;
        let mut memory = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/proc/self/mem")?;
        let mut bytes = vec![0; length];
        memory.seek(SeekFrom::Start(start))?;
        memory.read_exact(&mut bytes)?;
        let blanked = blank(&mut bytes, texts);
        if blanked > 0 {
            memory.seek(SeekFrom::Start(start))?;
            memory.write_all(&bytes)?;
        }
        Ok(blanked)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = texts;
        Ok(0)
    }
}

/// The `arg_start` and `arg_end` addresses (fields 48 and 49) of a `/proc/<pid>/stat` line. The
/// command name in field 2 may contain spaces and parentheses, so fields are counted after its
/// last `)`, where field 3 begins.
#[cfg(any(target_os = "linux", test))]
fn argument_range(stat: &str) -> Option<(u64, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(48 - 3);
    let start = fields.next()?.parse().ok()?;
    let end = fields.next()?.parse().ok()?;
    (start < end).then_some((start, end))
}

/// Overwrites with spaces each NUL-terminated argument in `bytes` equal to one of `texts`.
#[cfg(any(target_os = "linux", test))]
fn blank(bytes: &mut [u8], texts: &[&str]) -> usize {
    let mut blanked = 0;
    let mut start = 0;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map_or(bytes.len(), |offset| start + offset);
        let argument = &mut bytes[start..end];
        if !argument.is_empty() && texts.iter().any(|text| text.as_bytes() == &argument[..]) {
            argument.fill(b' ');
            blanked += 1;
        }
        start = end + 1;
    }
    blanked
}

#[cfg(test)]
mod tests;
