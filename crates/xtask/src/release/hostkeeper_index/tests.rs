//! The parts of the index verb that need no files.

use super::*;

/// Catches an entry whose keys are out of order or that names another file's bytes: the text
/// is hostkeeper's canonical form, which `tests/hostkeeper_package` checks against its parser.
#[test]
fn an_entry_is_an_object_with_its_keys_in_order_and_the_digest_and_size_of_the_bytes() {
    assert_eq!(
        entry(Platform::LinuxArm64, "a.tar.zst", b"abc"),
        "{\"bytes\":3,\"file\":\"a.tar.zst\",\"platform\":\"linux-arm64\",\
         \"sha256\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"}"
    );
}

/// Catches a comparison that names only one side of the difference.
#[test]
fn the_problem_names_the_missing_and_the_unexpected_packages() {
    let wanted = vec![
        (Platform::LinuxArm64, "a.tar.zst".to_owned()),
        (Platform::LinuxX86_64, "b.tar.zst".to_owned()),
    ];
    let found = |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).to_owned()).collect() };
    assert_eq!(problem(&found(&["a.tar.zst", "b.tar.zst"]), &wanted), None);
    assert_eq!(
        problem(&found(&["a.tar.zst"]), &wanted).as_deref(),
        Some("missing b.tar.zst")
    );
    assert_eq!(
        problem(&found(&["a.tar.zst", "b.tar.zst", "c.tar.zst"]), &wanted).as_deref(),
        Some("unexpected c.tar.zst")
    );
    assert_eq!(
        problem(&found(&["a.tar.zst", "c.tar.zst"]), &wanted).as_deref(),
        Some("missing b.tar.zst; unexpected c.tar.zst")
    );
}
