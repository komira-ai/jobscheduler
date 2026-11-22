//! Whether a built `js-init` is what `build.rs` promises: a 64-bit little-endian ELF
//! executable that names no program interpreter, so that it starts in an image with no
//! C library and no dynamic loader.
//!
//! `-C target-feature=+crt-static` asks for a static link, and a linker that cannot make one
//! may still link dynamically and succeed (a toolchain whose `cc` is `zig cc` for a glibc
//! target does). That binary runs on the host that built it and fails in a job's container
//! whose image has no `/lib64/ld-linux-x86-64.so.2`, so `build.rs` refuses it here, by name.
//!
//! Shared by `build.rs` and `tests/static_elf.rs` (`#[path]`).

/// The program header type of an interpreter path (`PT_INTERP`).
const PT_INTERP: u64 = 3;

/// Why `elf` is not a statically linked 64-bit little-endian ELF executable, or `None` when
/// it is one.
pub fn not_static(elf: &[u8]) -> Option<&'static str> {
    if elf.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Some("it is not an ELF file");
    }
    if elf.get(4..6) != Some([2u8, 1].as_slice()) {
        return Some("it is not a 64-bit little-endian ELF file");
    }
    let (Some(table), Some(size), Some(count)) =
        (field(elf, 32, 8), field(elf, 54, 2), field(elf, 56, 2))
    else {
        return Some("its ELF header is cut short");
    };
    for index in 0..count {
        match field(elf, table.saturating_add(index.saturating_mul(size)), 4) {
            None => return Some("its program headers run past the end of the file"),
            Some(PT_INTERP) => {
                return Some("it names a program interpreter: it is linked dynamically");
            }
            Some(_) => {}
        }
    }
    None
}

/// The little-endian unsigned number of `len` bytes at offset `at` of `elf`, or `None` when
/// the file ends before it does.
fn field(elf: &[u8], at: u64, len: u64) -> Option<u64> {
    let end = at.saturating_add(len);
    if end > elf.len() as u64 {
        return None;
    }
    Some(
        elf[at as usize..end as usize]
            .iter()
            .rev()
            .fold(0, |number, byte| number << 8 | u64::from(*byte)),
    )
}
