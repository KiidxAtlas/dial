use super::*;

#[test]
fn the_running_test_binary_is_summarized() {
    let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let out = read(&bytes).unwrap();
    let format = if cfg!(target_os = "macos") {
        "MachO"
    } else if cfg!(windows) {
        "Pe"
    } else {
        "Elf"
    };
    assert!(out.starts_with(format), "{}", &out[..out.len().min(400)]);
    assert!(out.contains("Sections ("), "{}", &out[..out.len().min(400)]);
    assert!(out.contains("Defined symbols ("));
    if cfg!(target_os = "linux") {
        assert!(out.contains("Needed libraries:") || out.contains("static"));
    }
}

#[test]
fn minimal_elf_header_parses() {
    let mut elf = vec![0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2; // 64-bit
    elf[5] = 1; // little-endian
    elf[6] = 1; // version
    elf[16] = 2; // ET_EXEC
    elf[18] = 0x3e; // x86-64
    elf[20] = 1;
    elf[24..32].copy_from_slice(&0x401000u64.to_le_bytes());
    elf[52] = 64; // e_ehsize
    let out = read(&elf).unwrap();
    assert!(
        out.starts_with("Elf Executable 64-bit little-endian, X86_64"),
        "{out}"
    );
    assert!(out.contains("Entry point: 0x401000"), "{out}");
}

#[test]
fn ar_archives_list_members() {
    let mut ar = b"!<arch>\n".to_vec();
    let data = b"hello\n";
    ar.extend_from_slice(
        format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "hello.o/",
            0,
            0,
            0,
            644,
            data.len()
        )
        .as_bytes(),
    );
    ar.extend_from_slice(data);
    let out = read(&ar).unwrap();
    assert!(out.contains("hello.o\t6 bytes"), "{out}");
}

#[test]
fn non_objects_are_refused() {
    assert!(read(b"plain text").is_err());
}
