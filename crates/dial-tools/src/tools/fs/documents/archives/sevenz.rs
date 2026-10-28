use super::{Walk, select_text};
use sevenz_rust2::{ArchiveReader, Error, Password};
use std::io::{self, Read, Seek};

pub(super) fn read<R: Read + Seek>(source: R, member: Option<&str>) -> Result<String, String> {
    let mut reader = ArchiveReader::new(source, Password::empty())
        .map_err(|e| format!("invalid 7z archive: {e}"))?;
    let mut walk = Walk::new(member);
    reader
        .for_each_entries(|entry, data| {
            if entry.is_anti_item() {
                return Err(Error::Other(
                    "7z anti-items (deletion markers) are not supported".into(),
                ));
            }
            let size = if entry.is_directory() {
                0
            } else {
                entry.size()
            };
            let selected = walk
                .admit(entry.name(), size)
                .map_err(|e| Error::Other(e.into()))?;
            if selected {
                select_text(&mut walk, !entry.is_directory(), size, &mut *data)
                    .map_err(|e| Error::Other(e.into()))?;
            }
            // Drain the stream so the decoder validates the whole folder, not just selected data.
            io::copy(data, &mut io::sink())
                .map_err(|e| Error::Other(format!("reading 7z content: {e}").into()))?;
            Ok(true)
        })
        .map_err(|e| format!("reading 7z archive: {e}"))?;
    walk.finish()
}
