//! The host group's config package, built as the group's own CI builds one: a payload of
//! `/etc/js-agent/flags` (written as `/private/etc/...`, the real path of `/etc` on a Mac), packed
//! by `pkgbuild` and wrapped by `productbuild`, with no script, as the agent's own package is.
//! The payload also holds the hook's launcher ([`CONFIG_HOOK`]), which hostkeeper at the pinned
//! revision needs in the package's tree (see [`crate::hostkeeper::CONFIG`]).

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hostkeeper::{CONFIG_HOOK, CONFIG_SERVICE};

/// The launcher: runs the installed agent's `config-hook` with the arguments the step runner
/// gives (`config-hook <step> [<release>] --params <file>`).
const LAUNCHER: &str =
    "#!/bin/sh\nexec \"/Library/Application Support/js-agent/bin/js-agent\" \"$@\"\n";

/// Writes `contents` at `path`, mode `mode`, its parents made 0755.
fn write(path: &Path, contents: &str, mode: u32) {
    let parent = path.parent().expect("a parent");
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(parent)
        .unwrap_or_else(|e| panic!("make {}: {e}", parent.display()));
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// Runs the system tool `program` with `args`, which must succeed.
fn run(program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The config package of `version` that installs `flags` as `/etc/js-agent/flags`, built in the
/// empty directory `dir`; the path of the package.
pub fn build(dir: &Path, flags: &str, version: &str) -> PathBuf {
    let root = dir.join("root");
    write(&root.join("private/etc/js-agent/flags"), flags, 0o644);
    write(
        &root.join(CONFIG_HOOK.trim_start_matches('/')),
        LAUNCHER,
        0o755,
    );
    let component = dir.join("component.pkg");
    let pkg = dir.join(format!("{CONFIG_SERVICE}-{version}.pkg"));
    let text = |path: &Path| path.to_str().expect("a UTF-8 path").to_owned();
    run(
        "/usr/bin/pkgbuild",
        &[
            "--root",
            &text(&root),
            "--identifier",
            CONFIG_SERVICE,
            "--version",
            version,
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            &text(&component),
        ],
    );
    run(
        "/usr/bin/productbuild",
        &[
            "--package",
            &text(&component),
            "--version",
            version,
            &text(&pkg),
        ],
    );
    pkg
}
