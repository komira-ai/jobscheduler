//! Mounts, and the roots they resolve against, on a real directory tree with real links.

use std::fs;
use std::os::unix::fs::symlink;

use js_agent::lifecycle::{MountError, MountRoots, ScratchRoot};
use js_agent_wire::{Mount, Outcome};

use super::{Harness, job, names};
use crate::support::Scratch;

fn mount(root: &str, rel: &str) -> Mount {
    Mount {
        root_name: root.to_owned(),
        rel: rel.to_owned(),
        ro: true,
    }
}

/// A root `data` with a directory `ok/inner`, a link `up` out of it to the sibling
/// `outside`, a link `down` inside it, and a plain file `file`.
fn tree(name: &str) -> (Scratch, MountRoots) {
    let dir = Scratch::new(name);
    for sub in ["data/ok/inner", "outside/secret"] {
        fs::create_dir_all(dir.path(sub)).unwrap();
    }
    symlink(dir.path("outside"), dir.path("data/up")).unwrap();
    symlink(dir.path("data/ok"), dir.path("data/down")).unwrap();
    fs::write(dir.path("data/file"), "x").unwrap();
    let roots = MountRoots::new(&[("data".to_owned(), dir.path("data"))]).unwrap();
    (dir, roots)
}

/// Asserts that a path below the root resolves to its canonical host path, through a
/// link that stays inside the root, with the read-only flag kept and the container path
/// built from the root's name; and that a file can be mounted too. It catches a resolver
/// that refuses what is legitimate.
#[test]
fn a_path_below_the_root_resolves_to_its_canonical_host_path() {
    let (dir, roots) = tree("mounts-ok");
    let canonical = fs::canonicalize(dir.path("data/ok/inner")).unwrap();
    let plain = roots.resolve(&mount("data", "ok/inner")).unwrap();
    assert_eq!(plain.host, canonical);
    assert_eq!(plain.target, "/mnt/data/ok/inner");
    assert!(plain.ro);
    let through = roots.resolve(&mount("data", "down/inner")).unwrap();
    assert_eq!(
        through.host, canonical,
        "a link that stays inside is followed"
    );
    let file = roots.resolve(&mount("data", "file")).unwrap();
    assert_eq!(file.host, fs::canonicalize(dir.path("data/file")).unwrap());
    assert_eq!(
        roots.root("data"),
        Some(fs::canonicalize(dir.path("data")).unwrap().as_path())
    );
    assert_eq!(roots.root("nope"), None);
}

/// Asserts that a mount outside every root is refused: a root name the agent does not
/// have, a path that does not exist, and an absolute path. It catches a mount of
/// anything the operator did not name.
#[test]
fn mount_outside_root_refused() {
    let (_dir, roots) = tree("mounts-outside");
    assert_eq!(
        roots.resolve(&mount("other", "ok")),
        Err(MountError::UnknownRoot)
    );
    assert_eq!(
        roots.resolve(&mount("data", "missing")),
        Err(MountError::Unresolvable)
    );
    assert_eq!(
        roots.resolve(&mount("data", "/etc")),
        Err(MountError::Escapes)
    );
}

/// Asserts that a link inside the root that leads out of it is refused, whether the
/// mount names the link itself or goes through it, and that the refusal does not depend
/// on what is behind the link. It catches the escape the wire text cannot show.
#[test]
fn symlink_escape_refused() {
    let (dir, roots) = tree("mounts-symlink");
    for rel in ["up", "up/secret"] {
        assert_eq!(
            roots.resolve(&mount("data", rel)),
            Err(MountError::Escapes),
            "{rel}"
        );
    }
    let outside = dir.path("outside");
    symlink(&outside, dir.path("data/ok/inner/again")).unwrap();
    assert_eq!(
        roots.resolve(&mount("data", "ok/inner/again/secret")),
        Err(MountError::Escapes)
    );
}

/// Asserts that a `..` that climbs out of the root is refused even when the message is
/// not validated first (the wire validator refuses these texts; the resolver does not
/// rely on that), and that a `..` that stays inside the root is allowed to resolve. It
/// catches a resolver that trusts the text.
#[test]
fn dotdot_escape_refused() {
    let (_dir, roots) = tree("mounts-dotdot");
    for rel in ["..", "../outside", "ok/../../outside/secret"] {
        assert_eq!(
            roots.resolve(&mount("data", rel)),
            Err(MountError::Escapes),
            "{rel}"
        );
    }
    assert!(roots.resolve(&mount("data", "ok/../ok/inner")).is_ok());
}

/// Asserts that the roots refuse what cannot be a root, when the agent starts: a name
/// the wire would not take, a name twice, a path that does not exist and one that is a
/// file; and that a root given as a link is stored as the directory it leads to. It
/// catches a root that is only found to be wrong when a job needs it.
#[test]
fn roots_are_checked_when_the_agent_starts() {
    let (dir, _) = tree("mounts-roots");
    let entry = |name: &str, path: &str| (name.to_owned(), dir.path(path));
    let refused = |entries: &[(String, std::path::PathBuf)]| MountRoots::new(entries).unwrap_err();
    assert!(refused(&[entry("../x", "data")]).contains("not a usable root name"));
    assert!(refused(&[entry("a", "data"), entry("a", "outside")]).contains("given twice"));
    assert!(refused(&[entry("a", "nowhere")]).contains("not an existing directory"));
    assert!(refused(&[entry("a", "data/file")]).contains("not an existing directory"));
    let linked = MountRoots::new(&[entry("a", "data/down")]).unwrap();
    assert_eq!(
        linked.root("a"),
        Some(fs::canonicalize(dir.path("data/ok")).unwrap().as_path())
    );
    assert_eq!(MountRoots::new(&[]).unwrap(), MountRoots::default());
}

/// Asserts that every error says something fixed and short, and nothing of the mount.
#[test]
fn mount_errors_say_what_they_mean() {
    let texts = [
        MountError::UnknownRoot,
        MountError::Unresolvable,
        MountError::Escapes,
    ]
    .map(|why| why.to_string());
    assert_eq!(
        texts,
        [
            "the mount names a root this agent does not have",
            "the mount path cannot be resolved below its root",
            "the mount path leaves its root"
        ]
    );
}

/// Asserts, through a whole start, that a mount that leaves its root (a link to a
/// directory outside it) or names no root is refused with `invalid-mount`, creates no
/// container and no scratch directory, and holds a `failed_to_start` result; and that a
/// good mount reaches the runtime resolved, with the scratch directory beside it. It
/// catches a start that mounts an escaping path.
#[test]
fn a_start_with_an_escaping_mount_creates_nothing() {
    let mut h = Harness::new("mounts-start");
    let outside = h.dir.path("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, h.dir.path("data/up")).unwrap();
    fs::create_dir(h.dir.path("data/ok")).unwrap();
    for (run, root, rel) in [("esc", "data", "up"), ("noroot", "other", "ok")] {
        let mut spec = job(run);
        spec.mounts = vec![mount(root, rel)];
        let events = h.start(spec);
        assert_eq!(names(&events), [format!("refused {run} invalid-mount")]);
        assert!(!h.scratch_of(run).exists());
    }
    assert!(h.log.borrow().is_empty());
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("invalid-mount"));
    let mut good = job("good");
    good.mounts = vec![mount("data", "ok")];
    assert_eq!(names(&h.start(good)), ["started good"]);
    let made = &h.fake.created()[0];
    assert_eq!(
        made.mounts[0].host,
        fs::canonicalize(h.dir.path("data/ok")).unwrap()
    );
    assert_eq!(made.mounts[0].target, "/mnt/data/ok");
}

/// Asserts the scratch root's checks when the agent starts: it must be a directory
/// (not a link to one), owned by the agent's user, with no permission for group or
/// others; and that a good one makes each run's directory `0700` whatever the umask,
/// replaces a stale one, and removes a missing one without complaint. It catches a
/// scratch directory another user can enter.
#[test]
fn the_scratch_root_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("scratch-root");
    let root = dir.path("root");
    fs::create_dir(&root).unwrap();
    let mode = |path: &std::path::Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };
    mode(&root, 0o755);
    assert!(
        ScratchRoot::open(&root)
            .unwrap_err()
            .contains("no permission")
    );
    mode(&root, 0o770);
    assert!(ScratchRoot::open(&root).is_err());
    mode(&root, 0o700);
    let link = dir.path("link");
    symlink(&root, &link).unwrap();
    assert!(
        ScratchRoot::open(&link)
            .unwrap_err()
            .contains("not a directory")
    );
    fs::write(dir.path("file"), "x").unwrap();
    assert!(ScratchRoot::open(&dir.path("file")).is_err());
    assert!(
        ScratchRoot::open(&dir.path("absent"))
            .unwrap_err()
            .contains("cannot read")
    );
    assert!(
        ScratchRoot::open(std::path::Path::new("/"))
            .unwrap_err()
            .contains("not owned")
    );

    let scratch = ScratchRoot::open(&root).unwrap();
    let made = scratch.create("r1").unwrap();
    assert_eq!(made, root.join("r1"));
    let bits = fs::metadata(&made).unwrap().permissions().mode() & 0o777;
    assert_eq!(bits, 0o700);
    fs::write(made.join("leftover"), "x").unwrap();
    let again = scratch.create("r1").unwrap();
    assert!(
        !again.join("leftover").exists(),
        "a stale directory is replaced"
    );
    symlink(dir.path("file"), again.join("link")).unwrap();
    scratch.remove("r1").unwrap();
    assert!(!made.exists());
    assert!(
        dir.path("file").exists(),
        "a link inside is removed, not followed"
    );
    scratch.remove("r1").unwrap();
    fs::write(root.join("blocked"), "x").unwrap();
    assert!(scratch.remove("blocked").is_err());
    assert!(scratch.create("blocked").is_err());
}
