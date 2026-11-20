//! The Xcode build of the job (`Constraints.xcode`), held to the evolution rule of the
//! package: it was added as an optional field with a number of its own.

use js_proto::v1::{Constraints, ExecMode};
use prost::Message;

/// Asserts that the bytes of a `Constraints` written before the field existed (only the
/// fields numbered 1 to 3) decode with an empty `xcode`, and that a `Constraints` with a build
/// survives an encode and a decode, and is written under field number 8 as a string (tag byte
/// 0x42, length 8, the text), the bytes a previous release skips as an unknown field. Catches
/// a field that is renumbered, or one whose absence a reader refuses.
#[test]
fn a_constraints_without_xcode_decodes_empty_and_the_field_round_trips() {
    let old = [
        0x0a, 0x05, b'l', b'i', b'n', b'u', b'x', // os = 1
        0x12, 0x06, b'x', b'8', b'6', b'_', b'6', b'4', // arch = 2
        0x18, 0x01, // exec = 3 (container)
    ];
    let decoded = Constraints::decode(old.as_slice()).expect("the previous bytes decode");
    assert_eq!(decoded.os, "linux");
    assert_eq!(decoded.exec, ExecMode::Container as i32);
    assert_eq!(decoded.xcode, "");
    assert_eq!(decoded.encode_to_vec(), old);

    let asked = Constraints {
        xcode: "16C5032a".to_owned(),
        ..Constraints::default()
    };
    let bytes = asked.encode_to_vec();
    assert_eq!(
        bytes,
        [0x42, 0x08, b'1', b'6', b'C', b'5', b'0', b'3', b'2', b'a']
    );
    assert_eq!(Constraints::decode(bytes.as_slice()), Ok(asked));
}
