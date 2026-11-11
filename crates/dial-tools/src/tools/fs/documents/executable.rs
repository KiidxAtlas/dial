//! Executables, shared libraries, object files and static archives (ELF, Mach-O, PE/COFF, ar)
//! summarized from their headers and symbol tables. Nothing is loaded, linked or executed.

use object::{
    Endianness, FileKind, Object, ObjectSection, ObjectSymbol, SectionKind,
    elf::{DT_NEEDED, DT_RPATH, DT_RUNPATH, DT_SONAME},
    read::{
        ExportTarget, File, NameOrOrdinal,
        archive::ArchiveFile,
        elf::{Dyn, ElfFile, FileHeader, ProgramHeader},
        macho::{FatArch, MachOFatFile32, MachOFatFile64},
    },
};
use std::fmt::Write as _;

#[cfg(test)]
mod tests;

/// Most rows listed for each of sections, imports, exports and symbols.
const MAX_ROWS: usize = 20_000;

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    match FileKind::parse(bytes).map_err(|e| format!("unrecognized object file: {e}"))? {
        FileKind::Archive => archive(bytes),
        FileKind::MachOFat32 => fat(
            MachOFatFile32::parse(bytes)
                .map_err(|e| e.to_string())?
                .arches(),
            bytes,
        ),
        FileKind::MachOFat64 => fat(
            MachOFatFile64::parse(bytes)
                .map_err(|e| e.to_string())?
                .arches(),
            bytes,
        ),
        _ => single(bytes),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn archive(bytes: &[u8]) -> Result<String, String> {
    let archive = ArchiveFile::parse(bytes).map_err(|e| format!("invalid ar archive: {e}"))?;
    let mut out = format!("ar archive ({:?})\nMembers:\n", archive.kind());
    for (index, member) in archive.members().enumerate() {
        if index == MAX_ROWS {
            out.push_str("…[more members omitted]\n");
            break;
        }
        let member = member.map_err(|e| format!("invalid archive member: {e}"))?;
        let _ = writeln!(out, "  {}\t{} bytes", text(member.name()), member.size());
    }
    Ok(out)
}

fn fat<A: FatArch>(arches: &[A], bytes: &[u8]) -> Result<String, String> {
    let mut out = format!(
        "Mach-O universal binary with {} architectures\n",
        arches.len()
    );
    for arch in arches {
        let _ = writeln!(out, "\n== {:?} slice ==", arch.architecture());
        let slice = arch.data(bytes).map_err(|e| e.to_string())?;
        out.push_str(&single(slice)?);
    }
    Ok(out)
}

fn single(bytes: &[u8]) -> Result<String, String> {
    let file = File::parse(bytes).map_err(|e| format!("unrecognized object file: {e}"))?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:?} {:?} {}-bit {}, {:?}",
        file.format(),
        file.kind(),
        if file.is_64() { 64 } else { 32 },
        if file.is_little_endian() {
            "little-endian"
        } else {
            "big-endian"
        },
        file.architecture(),
    );
    if file.entry() != 0 {
        let _ = writeln!(out, "Entry point: {:#x}", file.entry());
    }
    if let Ok(Some(id)) = file.build_id() {
        let _ = writeln!(out, "Build ID: {}", hex(id));
    }
    if let Ok(Some(uuid)) = file.mach_uuid() {
        let _ = writeln!(out, "UUID: {}", hex(&uuid));
    }
    let _ = writeln!(
        out,
        "Debug symbols: {}",
        if file.has_debug_symbols() {
            "present"
        } else {
            "absent"
        }
    );
    match &file {
        File::Elf32(elf) => elf_dynamic(elf, &mut out),
        File::Elf64(elf) => elf_dynamic(elf, &mut out),
        _ => {}
    }
    let imports: Vec<_> = file
        .imports()
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default();
    let mut libraries: Vec<String> = imports
        .iter()
        .map(|i| text(i.library()))
        .filter(|l| !l.is_empty())
        .collect();
    libraries.sort();
    libraries.dedup();
    if !libraries.is_empty() {
        let _ = writeln!(out, "Linked libraries: {}", libraries.join(", "));
    }

    let sections: Vec<_> = file.sections().collect();
    let _ = writeln!(out, "\nSections ({}):", sections.len());
    for section in sections.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "  {:<24} {:>#12x} {:>10} bytes  {}",
            section.name().unwrap_or("?"),
            section.address(),
            section.size(),
            kind(section.kind()),
        );
    }

    let _ = writeln!(out, "\nImports ({}):", imports.len());
    for import in imports.iter().take(MAX_ROWS) {
        let name = match import.name() {
            NameOrOrdinal::Name(name) => text(name),
            NameOrOrdinal::Ordinal(n) => format!("#{n}"),
        };
        let library = text(import.library());
        if library.is_empty() {
            let _ = writeln!(out, "  {name}");
        } else {
            let _ = writeln!(out, "  {name}\t({library})");
        }
    }
    let exports: Vec<_> = file
        .exports()
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default();
    let _ = writeln!(out, "\nExports ({}):", exports.len());
    for export in exports.iter().take(MAX_ROWS) {
        let name = match export.name() {
            NameOrOrdinal::Name(name) => text(name),
            NameOrOrdinal::Ordinal(n) => format!("#{n}"),
        };
        match export.target() {
            ExportTarget::Address { address } => {
                let _ = writeln!(out, "  {address:#x}\t{name}");
            }
            ExportTarget::Absolute { value } => {
                let _ = writeln!(out, "  ={value:#x}\t{name}");
            }
            other => {
                let _ = writeln!(out, "  {other:?}\t{name}");
            }
        }
    }

    let defined: Vec<_> = file
        .symbols()
        .filter(|s| s.is_definition() && !s.name().unwrap_or("").is_empty())
        .collect();
    let _ = writeln!(out, "\nDefined symbols ({}):", defined.len());
    for symbol in defined.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "  {:#x}\t{:?}\t{}{}",
            symbol.address(),
            symbol.kind(),
            symbol.name().unwrap_or("?"),
            if symbol.is_global() { "" } else { " (local)" },
        );
    }
    if defined.len() > MAX_ROWS {
        out.push_str("…[more symbols omitted]\n");
    }
    Ok(out)
}

fn kind(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Text => "code",
        SectionKind::Data => "data",
        SectionKind::ReadOnlyData | SectionKind::ReadOnlyDataWithRel => "read-only data",
        SectionKind::ReadOnlyString => "strings",
        SectionKind::UninitializedData => "bss",
        SectionKind::Tls | SectionKind::UninitializedTls => "thread-local",
        SectionKind::Debug | SectionKind::DebugString => "debug",
        SectionKind::Note => "note",
        SectionKind::Linker | SectionKind::Metadata => "metadata",
        _ => "other",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Interpreter, soname, run paths and needed libraries from an ELF dynamic section.
fn elf_dynamic<Elf: FileHeader<Endian = Endianness>>(elf: &ElfFile<'_, Elf>, out: &mut String) {
    let (endian, data) = (elf.endian(), elf.data());
    for segment in elf.elf_program_headers() {
        if let Ok(Some(interpreter)) = segment.interpreter(endian, data) {
            let _ = writeln!(out, "Interpreter: {}", text(interpreter));
        }
    }
    let table = elf.elf_section_table();
    let Ok(Some((entries, link))) = table.dynamic(endian, data) else {
        return;
    };
    let Ok(strings) = table.strings(endian, data, link) else {
        return;
    };
    let mut needed = Vec::new();
    for entry in entries {
        let Ok(value) = entry.string(endian, strings) else {
            continue;
        };
        match entry.tag(endian) {
            DT_NEEDED => needed.push(text(value)),
            DT_SONAME => {
                let _ = writeln!(out, "Soname: {}", text(value));
            }
            DT_RPATH | DT_RUNPATH => {
                let _ = writeln!(out, "Run path: {}", text(value));
            }
            _ => {}
        }
    }
    if !needed.is_empty() {
        let _ = writeln!(out, "Needed libraries: {}", needed.join(", "));
    }
}
