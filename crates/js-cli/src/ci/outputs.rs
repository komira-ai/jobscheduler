//! What `ci run` does with a step's outputs and its end on the runner: unpacks the archive
//! the job wrote into the outputs directory, and appends the step's outputs for GitHub
//! Actions to the file `$GITHUB_OUTPUT` names.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Unpacks `archive`, a tar archive, into `dir`, which is made with its parents when
/// missing. Only files and directories are unpacked: an archive that holds a symbolic or a
/// hard link, a device or anything else is refused, because a link's target is the runner's
/// to read (an artifact upload follows it). The tar reader leaves out an entry whose path is
/// absolute or climbs out with `..`.
///
/// # Errors
///
/// The text of why the directory cannot be made or the archive cannot be unpacked.
pub(crate) fn unpack(archive: &[u8], dir: &Path) -> Result<(), String> {
    let place =
        |why: std::io::Error| format!("cannot unpack the outputs into {}: {why}", dir.display());
    fs::create_dir_all(dir).map_err(place)?;
    let mut reader = tar::Archive::new(archive);
    reader.set_preserve_permissions(false);
    reader.set_overwrite(true);
    for entry in reader.entries().map_err(place)? {
        let mut entry = entry.map_err(place)?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            return Err(format!(
                "cannot unpack the outputs into {}: {path} is not a file or a directory",
                dir.display()
            ));
        }
        entry.unpack_in(dir).map_err(place)?;
    }
    Ok(())
}

/// The step's outputs, one `name=value` line each: `job-id`, `outcome`, and `exit-code`
/// (empty when the job did not exit by itself).
pub(crate) fn github_lines(job: &str, outcome: &str, exit_code: Option<i32>) -> String {
    let code = exit_code.map_or_else(String::new, |code| code.to_string());
    format!("job-id={job}\noutcome={outcome}\nexit-code={code}\n")
}

/// Appends `lines` to the file at `path`, made when missing.
///
/// # Errors
///
/// The text of why the file cannot be opened or written.
pub(crate) fn append(path: &Path, lines: &str) -> Result<(), String> {
    let opened = OpenOptions::new().create(true).append(true).open(path);
    let written = opened.and_then(|mut file| file.write_all(lines.as_bytes()));
    written.map_err(|why| {
        format!(
            "cannot write the step's outputs to {}: {why}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use js_testkit::dir::TestDir;

    /// A tar archive of one entry of `kind` at `path`, holding `data` or linking to `target`.
    fn one(kind: tar::EntryType, path: &str, data: &[u8], target: Option<&str>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        if let Some(target) = target {
            header.set_link_name(target).unwrap();
        }
        header.set_path(path).unwrap();
        header.set_cksum();
        builder.append(&header, data).unwrap();
        builder.into_inner().unwrap()
    }

    /// Asserts that a file and a directory are unpacked, an empty archive makes the directory
    /// alone, and a symbolic link, a hard link and a device are each refused by their whole
    /// message with nothing written through them. It catches an output that points the
    /// artifact upload at the runner's own files, such as its token.
    #[test]
    fn only_files_and_directories_are_unpacked() {
        let scratch = TestDir::under(std::env::temp_dir(), "ci-unpack");
        let into = scratch.path().join("out");
        unpack(&one(tar::EntryType::Regular, "a/b.txt", b"hi", None), &into).unwrap();
        unpack(&one(tar::EntryType::Directory, "d", b"", None), &into).unwrap();
        assert_eq!(fs::read(into.join("a/b.txt")).unwrap(), b"hi");
        assert!(into.join("d").is_dir());
        let empty = scratch.path().join("empty");
        unpack(&[], &empty).unwrap();
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
        let refused = [
            (tar::EntryType::Symlink, "token", Some("/etc/hostname")),
            (tar::EntryType::Link, "hard", Some("a/b.txt")),
            (tar::EntryType::Char, "dev", None),
        ];
        for (kind, path, target) in refused {
            let got = unpack(&one(kind, path, b"", target), &into);
            let want = format!(
                "cannot unpack the outputs into {}: {path} is not a file or a directory",
                into.display()
            );
            assert_eq!(got, Err(want));
            assert!(fs::symlink_metadata(into.join(path)).is_err(), "{path}");
        }
    }

    /// Asserts the three lines and an empty exit code. It catches a name GitHub's
    /// `steps.<id>.outputs` would not find.
    #[test]
    fn the_github_lines_name_each_output() {
        assert_eq!(
            github_lines("j-1", "failed", Some(3)),
            "job-id=j-1\noutcome=failed\nexit-code=3\n"
        );
        assert_eq!(
            github_lines("j-1", "lost", None),
            "job-id=j-1\noutcome=lost\nexit-code=\n"
        );
    }
}
