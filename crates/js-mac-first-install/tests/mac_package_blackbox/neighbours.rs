//! What tierstore gives a Mac of the farm that the agent's flags name, stood in for: its account
//! `_tierstore` (and its group), the directory of its mount provider's socket, its put directory,
//! and its one mount of the node at the trees root, `/private/var/tierstore/fs`.
//!
//! The mount is a volume of its own: an APFS disk image the test makes in its scratch directory
//! and attaches on the trees root (`hdiutil attach -mountpoint`), as tierstore's NFS mount is a
//! file system of its own mounted there. What the agent asks of the trees root is that it be an
//! existing directory that is a `--mount-root-ro` and lies above or below no other mount root
//! (the start check, `crates/js-agent/src/native_fs.rs`); a job that mounts a tree reads below it
//! only what its provider made. No job here mounts a tree, so no provider serves the socket: the
//! agent connects to its provider only for a job that names one (that path is the black-box test
//! of the agent beside tierstore). "Missing" is what the Macs had: no volume and no directory at
//! the trees root.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, chown};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::look::output;

/// tierstore's account, which the flags name as the provider's and the put ingest's user.
pub const ACCOUNT: &str = "_tierstore";

/// The trees root: tierstore's mount, a `--mount-root-ro` and the `--native-trees-root`.
pub const TREES_ROOT: &str = "/private/var/tierstore/fs";

/// The directory of tierstore's provider socket.
const SOCKET_DIR: &str = "/usr/local/tierstore/run";

/// tierstore's put directory, the `--native-put-root`.
const PUT_ROOT: &str = "/usr/local/tierstore/put";

/// The size of the volume: it holds nothing but its own directory.
const VOLUME_SIZE: &str = "32m";

/// The first id tried for the account and its group: below the range of the agent's slot
/// accounts (700 to 799) and of people's accounts (501 and up), where a service account is.
const FIRST_ID: u32 = 290;

/// The stand-in, with its disk image.
pub struct Tierstore {
    image: PathBuf,
}

impl Tierstore {
    /// Makes the account, the socket and put directories and the volume, and attaches the volume
    /// on the trees root. `scratch` is where the disk image is made.
    pub fn serve(scratch: &Path) -> Tierstore {
        let id = free_id();
        let id_text = id.to_string();
        run(
            "/usr/sbin/dseditgroup",
            &["-o", "create", "-i", &id_text, ACCOUNT],
        );
        let user = format!("/Users/{ACCOUNT}");
        for (key, value) in [
            ("UniqueID", id_text.as_str()),
            ("PrimaryGroupID", id_text.as_str()),
            ("UserShell", "/usr/bin/false"),
            ("NFSHomeDirectory", "/var/empty"),
        ] {
            run("/usr/bin/dscl", &[".", "-create", &user, key, value]);
        }
        dir(Path::new(SOCKET_DIR), 0o755);
        dir(Path::new(PUT_ROOT), 0o755);
        chown(PUT_ROOT, Some(id), Some(id)).unwrap_or_else(|e| panic!("chown {PUT_ROOT}: {e}"));
        let image = scratch.join("tierstore-fs.dmg");
        let image_text = image.to_str().expect("a UTF-8 path").to_owned();
        run(
            "/usr/bin/hdiutil",
            &[
                "create",
                "-size",
                VOLUME_SIZE,
                "-fs",
                "APFS",
                "-volname",
                "tierstore-fs",
                &image_text,
            ],
        );
        let served = Tierstore { image };
        served.attach();
        served
    }

    /// The volume attached on the trees root, the directory made first.
    pub fn attach(&self) {
        dir(Path::new(TREES_ROOT), 0o755);
        let image = self.image.to_str().expect("a UTF-8 path");
        run(
            "/usr/bin/hdiutil",
            &["attach", "-nobrowse", "-mountpoint", TREES_ROOT, image],
        );
        assert!(self.mounted(), "the volume is mounted on {TREES_ROOT}");
    }

    /// The volume detached and the trees root's directory removed: the trees root is missing,
    /// as on the Macs whose tierstore had not mounted it.
    pub fn take_away(&self) {
        run("/usr/bin/hdiutil", &["detach", TREES_ROOT]);
        fs::remove_dir(TREES_ROOT).unwrap_or_else(|e| panic!("remove {TREES_ROOT}: {e}"));
        assert!(!Path::new(TREES_ROOT).exists(), "{TREES_ROOT} is gone");
    }

    /// Whether a file system other than the one above it is mounted on the trees root.
    fn mounted(&self) -> bool {
        output("/sbin/mount", &[]).contains(&format!(" on {TREES_ROOT} "))
    }
}

/// The first id from [`FIRST_ID`] that no account and no group of the local directory has.
fn free_id() -> u32 {
    let users = output("/usr/bin/dscl", &[".", "-list", "/Users", "UniqueID"]);
    let groups = output(
        "/usr/bin/dscl",
        &[".", "-list", "/Groups", "PrimaryGroupID"],
    );
    let taken = |id: u32| {
        users
            .lines()
            .chain(groups.lines())
            .any(|line| line.split_whitespace().last() == Some(id.to_string().as_str()))
    };
    (FIRST_ID..500)
        .find(|id| !taken(*id))
        .expect("a free id below 500")
}

/// `program` with `args`, which must succeed.
fn run(program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {}: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The directory `path`, root's, mode `mode`, with each missing parent (0755).
fn dir(path: &Path, mode: u32) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .unwrap_or_else(|e| panic!("make {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}
