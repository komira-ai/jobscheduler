//! File probes: a write in each [`Target`], a read of each named path (whether it opens and
//! its size, never its contents), the marker files `leave` keeps and `find` looks for, free
//! space, and making many small files.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use crate::host::{Named, Target};
use crate::outcome::{Line, Outcome};

/// The file name of a probe's file in a target: `.js-isolation-probe-<tag>`.
#[must_use]
pub fn marker_name(tag: &str) -> String {
    format!(".js-isolation-probe-{tag}")
}

/// What making the marker file in a target did.
struct Made {
    line: Line,
    /// The file, when it was made.
    file: Option<PathBuf>,
    /// The directory the probe made, and the directory above it that was there before.
    made_dir: Option<(PathBuf, PathBuf)>,
}

fn make(prefix: &str, target: &Target, tag: &str) -> Made {
    let name = format!("{prefix}:{}", target.name);
    let shown = target.dir.display().to_string();
    let mut made_dir = None;
    if !target.dir.is_dir() {
        if !target.make_dir {
            return Made {
                line: Line::new(name, Outcome::NotApplicable).detail(format!("{shown} is absent")),
                file: None,
                made_dir: None,
            };
        }
        let before = target
            .dir
            .ancestors()
            .find(|p| p.is_dir())
            .unwrap_or(Path::new("/"))
            .to_path_buf();
        if let Err(err) = fs::create_dir_all(&target.dir) {
            return Made {
                line: Line::refused(name, &err, format!("making {shown}")),
                file: None,
                made_dir: None,
            };
        }
        made_dir = Some((target.dir.clone(), before));
    }
    let file = target.dir.join(marker_name(tag));
    // The file is empty: its name carries the tag, so nothing is written that could fail
    // after it exists.
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file);
    if let Err(err) = opened {
        return Made {
            line: Line::refused(name, &err, shown),
            file: None,
            made_dir,
        };
    }
    let line = Line::new(name, Outcome::Allowed).detail(shown);
    Made {
        line,
        file: Some(file),
        made_dir,
    }
}

/// Removes what [`make`] made; a failure is added to the line's detail.
fn undo(mut made: Made) -> Line {
    let mut failed = Vec::new();
    if let Some(file) = &made.file
        && let Err(err) = fs::remove_file(file)
    {
        failed.push(format!("could not remove {}: {err}", file.display()));
    }
    if let Some((dir, before)) = &made.made_dir
        && let Err(err) = remove_made_dirs(dir, before)
    {
        failed.push(format!("could not remove {}: {err}", dir.display()));
    }
    if !failed.is_empty() {
        made.line.detail = format!("{}; {}", made.line.detail, failed.join("; "));
    }
    made.line
}

/// Removes `dir` and its parents below `before`, the directory that was there before the
/// probe made them.
fn remove_made_dirs(dir: &Path, before: &Path) -> io::Result<()> {
    for made in dir.ancestors().take_while(|p| *p != before) {
        fs::remove_dir(made)?;
    }
    Ok(())
}

/// `write:<target>`: makes a file in the target and removes it again.
#[must_use]
pub fn write(target: &Target, tag: &str) -> Line {
    undo(make("write", target, tag))
}

/// `leave:file:<target>`: makes a file in the target and keeps it.
#[must_use]
pub fn leave(target: &Target, tag: &str) -> Line {
    make("leave:file", target, tag).line
}

/// `find:file:<target>`: whether the file `leave` made in the target (for the leaver's
/// account) is there, and, on the leaver's own slot, removes it.
#[must_use]
pub fn find(target: &Target, tag: &str, remove: bool) -> Line {
    let name = format!("find:file:{}", target.name);
    let file = target.dir.join(marker_name(tag));
    let shown = file.display().to_string();
    match fs::symlink_metadata(&file) {
        Ok(_) if remove => match fs::remove_file(&file) {
            Ok(()) => Line::new(name, Outcome::Found).detail(format!("{shown}, removed")),
            Err(err) => Line::new(name, Outcome::Found)
                .errno_of(&err)
                .detail(format!("{shown}, could not remove it")),
        },
        Ok(_) => Line::new(name, Outcome::Found).detail(format!("{shown}, left for its own slot")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Line::new(name, Outcome::Gone),
        Err(err) => Line::new(name, Outcome::NotApplicable)
            .errno_of(&err)
            .detail(format!("cannot look at {shown}")),
    }
}

/// `read:<name>`: opens the path (a directory is listed, a file opened and its size taken)
/// and says whether that worked, never what is in it.
#[must_use]
pub fn read(named: &Named) -> Line {
    let name = format!("read:{}", named.name);
    let path = Path::new(&named.value);
    let shown = path.display().to_string();
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Line::new(name, Outcome::NotApplicable).detail(format!("{shown} is absent"));
        }
        Err(err) => return Line::refused(name, &err, shown),
    };
    if meta.is_dir() {
        return match fs::read_dir(path) {
            Ok(entries) => Line::new(name, Outcome::Allowed)
                .detail(format!("{shown}: listed, {} entries", entries.count())),
            Err(err) => Line::refused(name, &err, shown),
        };
    }
    match fs::File::open(path).and_then(|f| f.metadata()) {
        Ok(meta) => Line::new(name, Outcome::Allowed)
            .detail(format!("{shown}: opened, {} bytes", meta.len())),
        Err(err) => Line::refused(name, &err, shown),
    }
}

/// `space:run-volume`: free space and free inodes of the volume `dir` is on.
#[must_use]
pub fn space(dir: &Path) -> Line {
    match rustix::fs::statvfs(dir) {
        Ok(v) => {
            let gib =
                |blocks: u64| (blocks.saturating_mul(v.f_frsize) as f64) / f64::from(1u32 << 30);
            Line::new("space:run-volume", Outcome::Info).detail(format!(
                "{:.1} GiB free of {:.1} GiB, {} files free of {}",
                gib(v.f_bavail),
                gib(v.f_blocks),
                v.f_favail,
                v.f_files
            ))
        }
        Err(err) => Line::refused(
            "space:run-volume",
            &io::Error::from(err),
            dir.display().to_string(),
        ),
    }
}

/// `files:<count>`: makes `count` small files in a new directory under `dir`, then removes
/// them and the directory.
#[must_use]
pub fn many(dir: &Path, tag: &str, count: u32) -> Line {
    let name = format!("files:{count}");
    let root = dir.join(format!("{}-files", marker_name(tag)));
    if let Err(err) = fs::create_dir(&root) {
        return Line::refused(name, &err, format!("making {}", root.display()));
    }
    let (made, failure) = fill(&root, count);
    let cleanup = cleanup(fs::remove_dir_all(&root), &root);
    summary(name, made, failure.as_ref(), &cleanup)
}

/// Makes the files `f00000`.. in `root`: how many were made, and the error that stopped it.
fn fill(root: &Path, count: u32) -> (u32, Option<io::Error>) {
    for n in 0..count {
        if let Err(err) = fs::write(root.join(format!("f{n:05}")), b"x") {
            return (n, Some(err));
        }
    }
    (count, None)
}

fn cleanup(removed: io::Result<()>, root: &Path) -> String {
    match removed {
        Ok(()) => "removed".to_owned(),
        Err(err) => format!("could not remove {}: {err}", root.display()),
    }
}

fn summary(name: String, made: u32, failure: Option<&io::Error>, cleanup: &str) -> Line {
    match failure {
        None => Line::new(name, Outcome::Allowed).detail(format!("{made} made, {cleanup}")),
        Some(err) => Line::refused(
            name,
            err,
            format!("{made} made before it failed, {cleanup}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use js_testkit::dir::TestDir;

    use super::*;

    fn target(name: &str, dir: &Path, make_dir: bool) -> Target {
        Target {
            name: name.to_owned(),
            dir: dir.to_path_buf(),
            make_dir,
        }
    }

    #[test]
    fn a_write_in_a_writable_dir_is_allowed_and_leaves_nothing() {
        let dir = TestDir::under(std::env::temp_dir(), "files-write");
        let line = write(&target("run-dir", &dir, false), "t1");
        assert_eq!(line.outcome, Outcome::Allowed, "{line}");
        assert_eq!(line.name, "write:run-dir");
        assert_eq!(fs::read_dir(&*dir).unwrap().count(), 0);
    }

    #[test]
    fn an_absent_dir_is_not_applicable_unless_the_probe_makes_it() {
        let dir = TestDir::under(std::env::temp_dir(), "files-absent");
        let absent = dir.join("absent");
        let line = write(&target("opt-homebrew", &absent, false), "t1");
        assert_eq!(line.outcome, Outcome::NotApplicable);
        assert!(line.detail.ends_with("is absent"), "{line}");
        let made = write(
            &target(
                "home-launchagents",
                &absent.join("Library/LaunchAgents"),
                true,
            ),
            "t1",
        );
        assert_eq!(made.outcome, Outcome::Allowed, "{made}");
        assert!(!absent.exists(), "the made directories are removed again");
        assert!(dir.exists(), "a directory the probe did not make is kept");
    }

    #[test]
    fn a_dir_that_cannot_be_made_or_written_is_reported_with_its_errno() {
        let dir = TestDir::under(std::env::temp_dir(), "files-refused");
        let file = dir.join("a-file");
        fs::write(&file, b"").unwrap();
        let line = write(&target("home-preferences", &file.join("sub"), true), "t1");
        assert_eq!(line.outcome, Outcome::Error);
        assert_eq!(line.errno, Some(libc::ENOTDIR));
        assert!(line.detail.starts_with("making "), "{line}");
        // An existing marker of the same tag: the probe never overwrites.
        fs::write(dir.join(marker_name("t2")), b"").unwrap();
        let line = write(&target("run-dir", &dir, false), "t2");
        assert_eq!(line.errno, Some(libc::EEXIST));
        assert_eq!(line.outcome, Outcome::Error);
    }

    #[test]
    fn undo_reports_what_it_could_not_remove() {
        let dir = TestDir::under(std::env::temp_dir(), "files-undo");
        let made = Made {
            line: Line::new("write:x", Outcome::Allowed).detail("d"),
            file: Some(dir.join("never-made")),
            made_dir: Some((dir.join("never-made-dir"), dir.to_path_buf())),
        };
        let line = undo(made);
        assert!(line.detail.starts_with("d; could not remove "), "{line}");
        assert!(dir.exists());
        assert_eq!(line.detail.matches("could not remove").count(), 2, "{line}");
    }

    #[test]
    fn a_left_marker_is_found_and_removed_on_its_own_slot_only() {
        let dir = TestDir::under(std::env::temp_dir(), "files-find");
        let t = target("run-dir", &dir, false);
        assert_eq!(leave(&t, "t3").outcome, Outcome::Allowed);
        assert_eq!(leave(&t, "t3").name, "leave:file:run-dir");
        let kept = find(&t, "t3", false);
        assert_eq!(kept.outcome, Outcome::Found);
        assert!(kept.detail.ends_with("left for its own slot"), "{kept}");
        let removed = find(&t, "t3", true);
        assert!(removed.detail.ends_with(", removed"), "{removed}");
        assert_eq!(find(&t, "t3", true).outcome, Outcome::Gone);
    }

    #[test]
    fn a_marker_that_cannot_be_looked_at_or_removed_says_so() {
        let dir = TestDir::under(std::env::temp_dir(), "files-find-odd");
        // A path under a regular file cannot be looked at (ENOTDIR).
        fs::write(dir.join("plain"), b"").unwrap();
        let t = target("x", &dir.join("plain"), false);
        let line = find(&t, "t4", false);
        assert_eq!(line.outcome, Outcome::NotApplicable);
        assert_eq!(line.errno, Some(libc::ENOTDIR));
        // A marker that is a directory is found but cannot be removed as a file.
        fs::create_dir(dir.join(marker_name("t5"))).unwrap();
        let line = find(&target("y", &dir, false), "t5", true);
        assert_eq!(line.outcome, Outcome::Found);
        assert!(line.detail.ends_with("could not remove it"), "{line}");
        assert!(line.errno.is_some());
    }

    #[test]
    fn a_read_says_whether_it_opened_and_the_size_never_the_contents() {
        let dir = TestDir::under(std::env::temp_dir(), "files-read");
        let file = dir.join("secret");
        fs::write(&file, b"do-not-print").unwrap();
        let named = |v: &Path| Named {
            name: "agent-token".to_owned(),
            value: v.display().to_string(),
        };
        let line = read(&named(&file));
        assert_eq!(
            line.to_string(),
            format!(
                "read:agent-token: allowed; {}: opened, 12 bytes",
                file.display()
            )
        );
        let line = read(&named(&dir));
        assert_eq!(line.outcome, Outcome::Allowed);
        assert!(line.detail.ends_with("listed, 1 entries"), "{line}");
        assert_eq!(
            read(&named(&dir.join("absent"))).outcome,
            Outcome::NotApplicable
        );
        let line = read(&named(&file.join("under-a-file")));
        assert_eq!(line.errno, Some(libc::ENOTDIR));
    }

    #[test]
    fn a_read_of_a_closed_file_or_dir_is_refused() {
        let dir = TestDir::under(std::env::temp_dir(), "files-read-closed");
        let file = dir.join("closed");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let sub = dir.join("closed-dir");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
        let named = |v: &Path| Named {
            name: "n".to_owned(),
            value: v.display().to_string(),
        };
        let root = rustix::process::geteuid().is_root();
        for path in [&file, &sub] {
            let line = read(&named(path));
            // Root opens anything: the mode refuses every other user.
            let want = [Outcome::Denied, Outcome::Allowed][usize::from(root)];
            assert_eq!(line.outcome, want, "{line}");
        }
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn space_is_information_about_the_volume() {
        let dir = TestDir::under(std::env::temp_dir(), "files-space");
        let line = space(&dir);
        assert_eq!(line.outcome, Outcome::Info);
        assert!(line.detail.contains("GiB free of"), "{line}");
        let line = space(&dir.join("absent"));
        assert_eq!(line.outcome, Outcome::Error);
        assert_eq!(line.errno, Some(libc::ENOENT));
    }

    #[test]
    fn many_files_are_made_and_removed() {
        let dir = TestDir::under(std::env::temp_dir(), "files-many");
        let line = many(&dir, "t6", 25);
        assert_eq!(line.to_string(), "files:25: allowed; 25 made, removed");
        assert_eq!(fs::read_dir(&*dir).unwrap().count(), 0);
        let line = many(&dir.join("absent"), "t6", 25);
        assert_eq!(line.errno, Some(libc::ENOENT));
    }

    #[test]
    fn filling_stops_at_the_first_failure() {
        let dir = TestDir::under(std::env::temp_dir(), "files-fill");
        // A directory where the third file goes makes the third write fail.
        fs::create_dir(dir.join("f00002")).unwrap();
        let (made, failure) = fill(&dir, 5);
        assert_eq!(made, 2);
        let failure = failure.unwrap();
        let line = summary("files:5".to_owned(), made, Some(&failure), "removed");
        assert_eq!(line.outcome, Outcome::Error, "{line}");
        assert_eq!(line.detail, "2 made before it failed, removed");
        let line = summary(
            "files:5".to_owned(),
            1,
            Some(&io::Error::from_raw_os_error(libc::EDQUOT)),
            "removed",
        );
        assert_eq!(
            line.to_string(),
            "files:5: error (EDQUOT); 1 made before it failed, removed"
        );
    }

    #[test]
    fn a_cleanup_that_fails_is_named() {
        let text = cleanup(
            Err(io::Error::from_raw_os_error(libc::EACCES)),
            Path::new("/r"),
        );
        assert!(text.starts_with("could not remove /r: "), "{text}");
    }
}
