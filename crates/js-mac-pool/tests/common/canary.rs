//! Canary files that a test makes in world-writable directories (`/Users/Shared`,
//! `/private/var/tmp`), possibly as root. Another user can pre-place a link at any name
//! there, so a file is made only with `O_CREAT | O_EXCL` (which refuses any existing
//! entry, a link included, without following it), under a name nobody can predict, and a
//! path is recorded for removal only when this call made the file.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// Makes `path` with `content`, failing if anything is there, a symbolic link included.
pub fn create_exclusive(path: &Path, content: &str) -> std::io::Result<()> {
    let mut file = open_exclusive(path)?;
    file.write_all(content.as_bytes())
}

fn open_exclusive(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
}

/// Eight random bytes as hex, from the system's source.
pub fn unpredictable() -> String {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .expect("the random device")
        .read_exact(&mut bytes)
        .expect("random bytes");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Removes what it holds when dropped, including when a panic unwinds through it. Make it
/// before the first file, and record a file only after this call made it.
#[derive(Default)]
pub struct Cleanup {
    files: Vec<PathBuf>,
}

impl Cleanup {
    pub fn record(&mut self, path: PathBuf) {
        self.files.push(path);
    }

    /// Makes a file named `<prefix>-<random>` in `dir`, records it, and returns its path.
    /// A name that is taken (a planted entry, say) is skipped, never written through; any other
    /// error, from the open or the write, ends the call at once.
    pub fn create_unique(&mut self, dir: &Path, prefix: &str, content: &str) -> PathBuf {
        self.create_unique_with(dir, prefix, unpredictable, |file| {
            file.write_all(content.as_bytes())
        })
    }

    /// `create_unique` with the name source and the writer given, so a test can make a
    /// name collide or a write fail.
    pub fn create_unique_with(
        &mut self,
        dir: &Path,
        prefix: &str,
        mut name: impl FnMut() -> String,
        mut write: impl FnMut(&mut File) -> std::io::Result<()>,
    ) -> PathBuf {
        const TRIES: usize = 16;
        let mut last = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
        for _ in 0..TRIES {
            let path = dir.join(format!("{prefix}-{}", name()));
            let mut file = match open_exclusive(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = error;
                    continue;
                }
                Err(error) => panic!("cannot make a file in {}: {error}", dir.display()),
            };
            // The file is ours from here: the guard removes it however this call ends.
            self.record(path.clone());
            if let Err(error) = write(&mut file) {
                let _ = std::fs::remove_file(&path);
                panic!("cannot write {}: {error}", path.display());
            }
            return path;
        }
        panic!(
            "no unused name in {} after {TRIES} tries, last error: {last}",
            dir.display()
        );
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
    }
}
