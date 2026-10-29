//! Volumes and `clonefile` semantics.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use super::Scratch;
use super::sys::{clone_fd, clone_file, fs_type};
use crate::report::{Probe, Report};

fn dev_and_type(path: &Path) -> String {
    let dev = fs::metadata(path).map_or("?".to_owned(), |m| m.dev().to_string());
    let kind = fs_type(path).unwrap_or_else(|e| format!("error: {e}"));
    format!("dev {dev}, {kind}")
}

fn dev(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.dev())
}

pub fn probes(scratch: &Scratch, report: &mut Report) {
    // Reported: where the runner's temp and home live. A runtime that clones inputs from
    // the store needs them on one volume.
    let mut volumes = Probe::info("fs/volumes");
    for (name, path) in [
        ("work", scratch.work.as_path()),
        ("home", scratch.home.as_path()),
        ("private_var", Path::new("/private/var")),
        ("private_tmp", Path::new("/private/tmp")),
        ("root", Path::new("/")),
    ] {
        volumes = volumes.fact(name, dev_and_type(path));
    }
    let same = dev(&scratch.work) == dev(&scratch.home);
    report.push(volumes.fact("work_and_home_share_a_volume", same));

    let dir = &scratch.base;
    let src = dir.join("clone-src");
    let payload: Vec<u8> = (0..65_536u32).map(|i| (i % 251) as u8).collect();
    if let Err(err) = fs::write(&src, &payload) {
        report.push(Probe::asserted("fs/clone-setup", false).fact("error", err));
        return;
    }

    // clonefile within one volume: same bytes, then a write to the clone leaves the original.
    let dst = dir.join("clone-dst");
    let cloned = clone_file(&src, &dst);
    let same_bytes = fs::read(&dst).is_ok_and(|b| b == payload);
    let probe = Probe::asserted("fs/clonefile-file", cloned.is_ok() && same_bytes);
    report.push(match &cloned {
        Ok(()) => probe,
        Err(err) => probe.fact("error", err),
    });
    let wrote = fs::OpenOptions::new()
        .append(true)
        .open(&dst)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"tail"));
    let untouched = fs::read(&src).is_ok_and(|b| b == payload);
    report.push(Probe::asserted(
        "fs/clone-is-independent",
        wrote.is_ok() && untouched,
    ));

    // Cloning onto an existing path is refused, not overwritten.
    let again = clone_file(&src, &dst);
    let exists = again.as_ref().err().and_then(std::io::Error::raw_os_error) == Some(libc::EEXIST);
    report.push(Probe::asserted("fs/clonefile-refuses-existing", exists));

    // A directory tree.
    let tree = dir.join("clone-tree");
    let tree_clone = dir.join("clone-tree-copy");
    let tree_made =
        fs::create_dir_all(tree.join("a/b")).and_then(|()| fs::write(tree.join("a/b/f"), b"x"));
    let tree_cloned = tree_made.and_then(|()| clone_file(&tree, &tree_clone));
    let tree_ok =
        tree_cloned.is_ok() && fs::read(tree_clone.join("a/b/f")).is_ok_and(|b| b == b"x");
    report.push(Probe::asserted("fs/clonefile-directory", tree_ok));

    // fclonefileat from an open descriptor.
    let fd_dst = dir.join("clone-fd-dst");
    let fd_cloned = fs::File::open(&src).and_then(|f| clone_fd(&f, &fd_dst));
    let fd_ok = fd_cloned.is_ok() && fs::read(&fd_dst).is_ok_and(|b| b == payload);
    report.push(Probe::asserted("fs/fclonefileat", fd_ok));

    // Reported: the owner a clone gets (a root caller's is not probed: no sudo here).
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let owner = |p: &Path| fs::metadata(p).map_or("?".to_owned(), |m| m.uid().to_string());
    report.push(
        Probe::info("fs/clone-owner")
            .fact("caller_uid", uid)
            .fact("source_uid", owner(&src))
            .fact("clone_uid", owner(&dst)),
    );

    // Reported: a clone from the runner's temp into the home directory. It works when they
    // share a volume and is EXDEV when they do not.
    let across = scratch
        .home
        .join(format!("js-mac-probe-{}-across", std::process::id()));
    let result = clone_file(&src, &across);
    let _ = fs::remove_file(&across);
    let probe = Probe::info("fs/clone-work-to-home")
        .fact("same_volume", same)
        .fact("cloned", result.is_ok());
    report.push(match result {
        Ok(()) => probe,
        Err(err) => probe.fact("error", err),
    });
}
