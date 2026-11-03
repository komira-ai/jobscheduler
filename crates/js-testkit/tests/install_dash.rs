//! [`install_copy`] and [`install_script`] with paths that start with `-`, which only a
//! relative path can: the test works in a directory of its own, and so changes the working
//! directory of its process. It is the only test of this binary, so no other test runs where
//! it does not expect.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use js_testkit::spawn::{install_copy, install_script};

/// Catches a path starting with `-` read by `cp` or `chmod` as an option (`cp -from -to`
/// fails with an invalid option, and the install panics): each path is the file named.
#[test]
fn a_path_starting_with_a_dash_is_a_path_not_an_option() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("install-dash");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    std::env::set_current_dir(&dir).expect("into the dir");
    std::fs::write("-from", b"#!/bin/true\n").expect("source");
    install_copy(Path::new("-from"), Path::new("-to"), "750");
    assert_eq!(
        std::fs::read(dir.join("-to")).expect("copy"),
        b"#!/bin/true\n"
    );
    install_script(Path::new("-script"), "#!/bin/true\n", "700");
    assert_eq!(
        std::fs::read(dir.join("-script")).expect("script"),
        b"#!/bin/true\n"
    );
    let mode = |name: &str| {
        let meta = std::fs::metadata(dir.join(name)).expect("meta");
        meta.permissions().mode() & 0o7777
    };
    assert_eq!((mode("-to"), mode("-script")), (0o750, 0o700));
}
