use super::{Walk, select_text};
use std::io::{Read, Seek};

pub(super) fn read<R: Read + Seek>(source: R, member: Option<&str>) -> Result<String, String> {
    let mut cabinet = cab::Cabinet::new(source).map_err(|e| format!("invalid CAB archive: {e}"))?;
    let mut files = Vec::new();
    for folder in cabinet.folder_entries() {
        for file in folder.file_entries() {
            files.push((file.name().to_owned(), u64::from(file.uncompressed_size())));
        }
    }
    let mut walk = Walk::new(member);
    for (raw, size) in files {
        // CAB stores Windows separators; normalize so the same member paths work as for other formats.
        let name = raw.replace('\\', "/");
        if walk.admit(&name, size)? {
            let reader = cabinet
                .read_file(&raw)
                .map_err(|e| format!("reading CAB member {name:?}: {e}"))?;
            select_text(&mut walk, true, size, reader)?;
        }
    }
    walk.finish()
}
