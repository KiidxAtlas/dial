use super::{Walk, select_text};
use sevenz_rust::{Password, SevenZReader};
use std::io::{self, Read, Seek, SeekFrom};

pub(super) fn read<R: Read + Seek>(mut source: R, member: Option<&str>) -> Result<String, String> {
    let len = source
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("reading 7z archive: {e}"))?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("reading 7z archive: {e}"))?;
    let mut reader = SevenZReader::new(source, len, Password::empty())
        .map_err(|e| format!("invalid 7z archive: {e}"))?;
    let mut walk = Walk::new(member);
    reader
        .for_each_entries(|entry, data| {
            if entry.is_anti_item() {
                return Err(sevenz_rust::Error::other(
                    "7z anti-items (deletion markers) are not supported",
                ));
            }
            let size = if entry.is_directory() {
                0
            } else {
                entry.size()
            };
            let selected = walk
                .admit(entry.name(), size)
                .map_err(sevenz_rust::Error::other)?;
            if selected {
                select_text(&mut walk, !entry.is_directory(), size, &mut *data)
                    .map_err(sevenz_rust::Error::other)?;
            }
            // Drain the stream so the decoder validates the whole folder, not just selected data.
            io::copy(data, &mut io::sink())
                .map_err(|e| sevenz_rust::Error::other(format!("reading 7z content: {e}")))?;
            Ok(true)
        })
        .map_err(|e| format!("reading 7z archive: {e}"))?;
    walk.finish()
}
