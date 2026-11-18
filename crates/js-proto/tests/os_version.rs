//! The least operating-system version of the job (`Constraints.os_version`), held to the
//! evolution rule of the package: it was added as an optional field with a number of its own.

use js_proto::v1::{Constraints, ExecMode};
use prost::Message;

/// Asserts that the bytes of a `Constraints` written before the field existed (only the
/// fields numbered 1 to 3) decode with an empty `os_version`, and that a `Constraints` with a
/// version survives an encode and a decode, and is written under field number 7 as a string
/// (tag byte 0x3a, length 2, the text), the bytes a previous release skips as an unknown
/// field. Catches a field that is renumbered, or one whose absence a reader refuses.
#[test]
fn a_constraints_without_os_version_decodes_empty_and_the_field_round_trips() {
    let old = [
        0x0a, 0x05, b'l', b'i', b'n', b'u', b'x', // os = 1
        0x12, 0x06, b'x', b'8', b'6', b'_', b'6', b'4', // arch = 2
        0x18, 0x01, // exec = 3 (container)
    ];
    let decoded = Constraints::decode(old.as_slice()).expect("the previous bytes decode");
    assert_eq!(decoded.os, "linux");
    assert_eq!(decoded.exec, ExecMode::Container as i32);
    assert_eq!(decoded.os_version, "");
    assert_eq!(decoded.encode_to_vec(), old);

    let asked = Constraints {
        os_version: "26".to_owned(),
        ..Constraints::default()
    };
    let bytes = asked.encode_to_vec();
    assert_eq!(bytes, [0x3a, 0x02, b'2', b'6']);
    assert_eq!(Constraints::decode(bytes.as_slice()), Ok(asked));
}
