//! `init/static_elf.rs`, the check `build.rs` applies to the `js-init` it built: an ELF file
//! is accepted only when it is a 64-bit little-endian executable that names no program
//! interpreter, and each other case is refused with its own reason.

#[path = "../init/static_elf.rs"]
mod static_elf;

use static_elf::not_static;

/// The program header type of an interpreter path.
const PT_INTERP: u32 = 3;
/// `PT_LOAD` and `PT_DYNAMIC`: what a static-pie executable has, with no `PT_INTERP`.
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

/// A 64-bit little-endian ELF header with its program header table at offset 64, holding one
/// 56-byte entry of each of `types`.
fn elf(types: &[u32]) -> Vec<u8> {
    let mut file = vec![0u8; 64];
    file[..4].copy_from_slice(b"\x7fELF");
    file[4] = 2;
    file[5] = 1;
    file[32..40].copy_from_slice(&64u64.to_le_bytes());
    file[54..56].copy_from_slice(&56u16.to_le_bytes());
    let count = u16::try_from(types.len()).expect("a few program headers");
    file[56..58].copy_from_slice(&count.to_le_bytes());
    for kind in types {
        let mut entry = vec![0u8; 56];
        entry[..4].copy_from_slice(&kind.to_le_bytes());
        file.extend(entry);
    }
    file
}

#[test]
fn an_executable_with_no_interpreter_is_static() {
    assert_eq!(not_static(&elf(&[PT_LOAD, PT_DYNAMIC, PT_LOAD])), None);
    assert_eq!(not_static(&elf(&[])), None);
}

#[test]
fn an_executable_that_names_an_interpreter_is_refused_as_dynamic() {
    let why = Some("it names a program interpreter: it is linked dynamically");
    assert_eq!(not_static(&elf(&[PT_INTERP, PT_LOAD])), why);
    assert_eq!(not_static(&elf(&[PT_LOAD, PT_DYNAMIC, PT_INTERP])), why);
}

#[test]
fn a_file_that_is_not_elf_is_refused() {
    let why = Some("it is not an ELF file");
    assert_eq!(not_static(b""), why);
    assert_eq!(not_static(b"\x7fEL"), why);
    assert_eq!(not_static(b"#!/bin/sh\nexit 0\n"), why);
}

#[test]
fn an_elf_file_of_another_class_or_byte_order_is_refused() {
    let why = Some("it is not a 64-bit little-endian ELF file");
    let mut thirty_two = elf(&[]);
    thirty_two[4] = 1;
    assert_eq!(not_static(&thirty_two), why);
    let mut big_endian = elf(&[]);
    big_endian[5] = 2;
    assert_eq!(not_static(&big_endian), why);
    assert_eq!(not_static(b"\x7fELF\x02"), why);
}

#[test]
fn a_header_cut_before_the_program_header_fields_is_refused() {
    let why = Some("its ELF header is cut short");
    let whole = elf(&[PT_LOAD]);
    assert_eq!(not_static(&whole[..39]), why);
    assert_eq!(not_static(&whole[..55]), why);
    assert_eq!(not_static(&whole[..57]), why);
}

#[test]
fn program_headers_past_the_end_of_the_file_are_refused() {
    let why = Some("its program headers run past the end of the file");
    let whole = elf(&[PT_LOAD, PT_LOAD]);
    assert_eq!(not_static(&whole[..whole.len() - 56 + 3]), why);
    let mut far = elf(&[PT_LOAD]);
    far[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(not_static(&far), why);
}

/// The `js-init` this build embeds (`src/oci/init.rs`) passed the same check in `build.rs`.
#[cfg(target_os = "linux")]
#[test]
fn the_init_this_build_embeds_is_static() {
    let init = include_bytes!(concat!(env!("OUT_DIR"), "/js-init"));
    assert_eq!(not_static(init), None);
}
