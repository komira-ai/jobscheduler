//! Canary files that a test makes in world-writable directories (`/Users/Shared`,
//! `/private/var/tmp`), possibly as root. Another user can pre-place a link at any name
//! there, so a file is made only with `O_CREAT | O_EXCL` (which refuses any existing
//! entry, a link included, without following it), under a name nobody can predict, and a
//! path is recorded for removal only when this call made the file.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// Makes `path` with `content`, failing if anything is there, a symbolic link included.
pub fn create_exclusive(path: &Path, content: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(content.as_bytes())
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
    /// A name that is taken (a planted entry, say) is skipped, never written through.
    pub fn create_unique(&mut self, dir: &Path, prefix: &str, content: &str) -> PathBuf {
        for _ in 0..16 {
            let path = dir.join(format!("{prefix}-{}", unpredictable()));
            if create_exclusive(&path, content).is_ok() {
                self.record(path.clone());
                return path;
            }
        }
        panic!("no unused name in {}", dir.display());
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
    }
}
