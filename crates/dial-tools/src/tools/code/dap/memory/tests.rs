use super::*;

#[test]
fn read_memory_arguments_require_a_reference_and_bounded_count() {
    assert_eq!(
        read_memory_arguments(&json!({"memoryReference":" 0x1000 ","count":16,"offset":-8}))
            .unwrap(),
        json!({"memoryReference":"0x1000","count":16,"offset":-8})
    );
    assert_eq!(
        read_memory_arguments(&json!({"memoryReference":"0x10","count":4,"offset":null})).unwrap(),
        json!({"memoryReference":"0x10","count":4}),
        "null optional fields are absent"
    );
    for (args, expected) in [
        (json!({"count":4}), "memoryReference is required"),
        (
            json!({"memoryReference":"  ","count":4}),
            "memoryReference is required",
        ),
        (
            json!({"memoryReference":"x".repeat(257),"count":4}),
            "longer than 256",
        ),
        (json!({"memoryReference":"0x10"}), "count is required"),
        (
            json!({"memoryReference":"0x10","count":0}),
            "between 1 and 4096",
        ),
        (
            json!({"memoryReference":"0x10","count":4097}),
            "between 1 and 4096",
        ),
        (
            json!({"memoryReference":"0x10","count":"4"}),
            "count must be an integer",
        ),
        (
            json!({"memoryReference":"0x10","count":4,"offset":1.5}),
            "offset must be an integer",
        ),
    ] {
        let error = read_memory_arguments(&args).unwrap_err();
        assert!(error.contains(expected), "{args}: {error}");
    }
}

#[test]
fn disassemble_arguments_validate_counts_offsets_and_symbols() {
    assert_eq!(
        disassemble_arguments(&json!({"memoryReference":"0x4000","instructionCount":8,
            "offset":4,"instructionOffset":-2,"resolveSymbols":true}))
        .unwrap(),
        json!({"memoryReference":"0x4000","instructionCount":8,"offset":4,
            "instructionOffset":-2,"resolveSymbols":true})
    );
    for (args, expected) in [
        (
            json!({"memoryReference":"0x4000"}),
            "instructionCount is required",
        ),
        (
            json!({"memoryReference":"0x4000","instructionCount":257}),
            "between 1 and 256",
        ),
        (
            json!({"memoryReference":"0x4000","instructionCount":2,"resolveSymbols":"yes"}),
            "resolveSymbols must be boolean",
        ),
    ] {
        let error = disassemble_arguments(&args).unwrap_err();
        assert!(error.contains(expected), "{args}: {error}");
    }
}

#[test]
fn memory_is_rendered_as_an_addressed_hex_and_ascii_dump() {
    let bytes: Vec<u8> = (b'A'..=b'Q').chain([0, 0x7f, b' ']).collect();
    let body = json!({"address":"0x1000","data":STANDARD.encode(&bytes),"unreadableBytes":3});
    let rendered = render_memory(body).unwrap();
    assert!(rendered.get("data").is_none(), "base64 is replaced");
    assert_eq!(rendered["bytes"], 20);
    assert_eq!(rendered["unreadableBytes"], 3, "other fields are kept");
    let lines: Vec<&str> = rendered["hex"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    assert_eq!(
        lines,
        [
            "0x0000000000001000  41 42 43 44 45 46 47 48 49 4a 4b 4c 4d 4e 4f 50  |ABCDEFGHIJKLMNOP|",
            "0x0000000000001010  51 00 7f 20                                      |Q.. |",
        ]
    );
}

#[test]
fn unaddressed_or_empty_memory_still_renders() {
    let rendered =
        render_memory(json!({"address":"opaque","data":STANDARD.encode([1u8, 2])})).unwrap();
    assert_eq!(
        rendered["hex"][0].as_str().unwrap().split("  ").next(),
        Some("+0x0000")
    );
    assert_eq!(parse_address("4096"), Some(4096));
    assert_eq!(parse_address("0XfF"), Some(255));
    let unreadable = json!({"address":"0x10","unreadableBytes":16});
    assert_eq!(render_memory(unreadable.clone()).unwrap(), unreadable);
    assert!(
        render_memory(json!({"address":"0x10","data":"!!"}))
            .unwrap_err()
            .contains("invalid base64")
    );
}
