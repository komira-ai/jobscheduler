//! `release pack --bins "<bin> ..." --target <triple> --version X.Y.Z --mtime-of <commit> --out
//! <dir>`: the release archive of each binary, `<out>/<bin>-<version>-<target>.tar.gz`, made of
//! the directory `release stage` lays down (in `--stage`), one command for what the workflow's
//! `tar` pipeline made of each.
//!
//! The archive depends only on the commit and the staged files: entries in name order, every
//! directory before what it holds; each entry's modification time the commit's committer time
//! (`git log -1 --format=%ct <commit>`); owner and group 0 with no names; mode 0755 for a
//! directory or an executable and 0644 for any other file, whatever the umask; and a gzip
//! header with no file name and no time. A re-run of a commit makes the same bytes.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::{Compression, GzBuilder};

use super::stage::{self, StageArgs};
use super::{Error, Version, io_error};

/// The arguments of `release pack`.
#[derive(clap::Args)]
pub struct PackArgs {
    /// The binaries, separated by spaces, each built at `target/<target>/release/<bin>`.
    #[arg(long)]
    pub bins: String,
    /// The target they were built for.
    #[arg(long)]
    pub target: String,
    /// The release version, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: Version,
    /// The commit whose committer time every entry of the archives carries.
    #[arg(long)]
    pub mtime_of: String,
    /// The directory the archives are written to; made if missing.
    #[arg(long)]
    pub out: PathBuf,
    /// The directory the archives' directories are staged in; made if missing.
    #[arg(long, default_value = "stage")]
    pub stage: PathBuf,
    /// The checkout the files are copied from, and whose git history holds the commit.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// The git program.
    #[arg(long, default_value = "git")]
    pub git: String,
}

/// The committer time of `--mtime-of`, in seconds since the epoch, as git in the checkout gives
/// it.
fn commit_time(a: &PackArgs) -> Result<u64, Error> {
    let args = ["log", "-1", "--format=%ct", &a.mtime_of];
    let command = format!("{} {}", a.git, args.join(" "));
    let out = Command::new(&a.git)
        .args(args)
        .current_dir(&a.repo_dir)
        .output()
        .map_err(|e| Error::Spawn {
            program: a.git.clone(),
            why: e.to_string(),
        })?;
    let printed = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(Error::Failed {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        });
    }
    printed.trim().parse().map_err(|_| Error::NotACount {
        command,
        got: printed.trim_end().to_owned(),
    })
}

/// A closure that makes the [`Error::Io`] of `path`.
fn at(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| io_error(path, &e)
}

/// The paths under `top` (a directory of `base`), relative to `base`, in name order with every
/// directory before what it holds.
fn entries(base: &Path, top: &str, out: &mut Vec<String>) -> Result<(), Error> {
    out.push(top.to_owned());
    let dir = base.join(top);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .and_then(|d| {
            d.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .map_err(at(&dir))?;
    names.sort();
    for name in names {
        let path = format!("{top}/{name}");
        if base.join(&path).is_dir() {
            entries(base, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// The `.tar.gz` of the directory `top` of `base`, every entry at `mtime`.
///
/// # Errors
/// [`Error::Io`] for a staged file that cannot be read.
pub fn archive(base: &Path, top: &str, mtime: u64) -> Result<Vec<u8>, Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut paths = Vec::new();
    entries(base, top, &mut paths)?;
    let gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(gz);
    for path in &paths {
        let local = base.join(path);
        let meta = std::fs::metadata(&local).map_err(at(&local))?;
        let mut header = tar::Header::new_gnu();
        header.set_mtime(mtime);
        header.set_uid(0);
        header.set_gid(0);
        // The modes the archives carry wherever they are made, whatever the umask: a directory
        // or an executable 0755, any other file 0644.
        let runs = meta.is_dir() || meta.permissions().mode() & 0o111 != 0;
        header.set_mode(if runs { 0o755 } else { 0o644 });
        let (name, bytes) = if meta.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            (format!("{path}/"), Vec::new())
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            (path.clone(), std::fs::read(&local).map_err(at(&local))?)
        };
        header.set_size(bytes.len() as u64);
        tar.append_data(&mut header, &name, bytes.as_slice())
            .map_err(at(&local))?;
    }
    let mut gz = tar.into_inner().map_err(at(base))?;
    gz.flush().map_err(at(base))?;
    gz.finish().map_err(at(base))
}

/// The verb: stages and archives each binary of `--bins`.
///
/// # Errors
/// [`Error`] for a commit whose time git does not give, a binary that cannot be staged, or an
/// archive that cannot be written.
pub fn run(a: &PackArgs) -> Result<String, Error> {
    let mtime = commit_time(a)?;
    std::fs::create_dir_all(&a.out).map_err(at(&a.out))?;
    let mut lines = Vec::new();
    for bin in a.bins.split_whitespace() {
        stage::run(&StageArgs {
            bin: bin.to_owned(),
            target: a.target.clone(),
            version: a.version,
            out: a.stage.clone(),
            repo_dir: a.repo_dir.clone(),
        })?;
        let name = format!("{bin}-{}-{}", a.version, a.target);
        let bytes = archive(&a.stage, &name, mtime)?;
        let file = a.out.join(format!("{name}.tar.gz"));
        std::fs::write(&file, &bytes).map_err(at(&file))?;
        lines.push(format!("packed {} ({} bytes)", file.display(), bytes.len()));
    }
    Ok(lines.join("\n"))
}
