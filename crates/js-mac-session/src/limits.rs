//! The limits a `run` request asks the helper to set on the process it starts.
//!
//! The helper sets them with `setrlimit` in the child, after the fork and before the
//! `exec` (see [`crate::spawn`]), soft and hard alike so the process cannot raise them.
//! They travel as fields of the request, in the units the kernel takes: the largest file
//! in bytes (`RLIMIT_FSIZE`) and the most processes the lease user may have
//! (`RLIMIT_NPROC`). A value is refused unless it is above zero (zero would forbid every
//! file or every process) and at most its cap (a limit above the cap is no limit, and the
//! kernel's "unlimited" is a value too).

use serde::{Deserialize, Serialize};

/// The largest `file_bytes` the helper accepts: 1 TiB.
pub const MAX_FILE_BYTES: u64 = 1 << 40;

/// The most `processes` the helper accepts.
pub const MAX_PROCESSES: u32 = 1 << 20;

/// The file-size and process limits of one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// The largest file the process may write, in bytes: `RLIMIT_FSIZE`.
    pub file_bytes: u64,
    /// The most processes the lease user may have: `RLIMIT_NPROC`.
    pub processes: u32,
}

impl Limits {
    /// The limits `file_bytes` and `processes`.
    ///
    /// # Errors
    /// A value is zero or above its cap ([`Limits::check`]).
    pub fn new(file_bytes: u64, processes: u32) -> Result<Self, String> {
        let limits = Self {
            file_bytes,
            processes,
        };
        limits.check().map(|()| limits)
    }

    /// Whether both values are above zero and within their caps. A request that arrives
    /// off the wire is checked with this: the types alone allow zero and anything up to
    /// `u64::MAX`, which `setrlimit` reads as unlimited.
    ///
    /// # Errors
    /// Which value is out of range.
    pub fn check(&self) -> Result<(), String> {
        if self.file_bytes == 0 || self.file_bytes > MAX_FILE_BYTES {
            return Err(format!(
                "the file size limit {} bytes is not in 1..={MAX_FILE_BYTES}",
                self.file_bytes
            ));
        }
        if self.processes == 0 || self.processes > MAX_PROCESSES {
            return Err(format!(
                "the process limit {} is not in 1..={MAX_PROCESSES}",
                self.processes
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a zero limit accepted (every write or every fork would fail), a limit
    /// past the cap accepted (`u64::MAX` is `RLIM_INFINITY`), or an edge value refused.
    #[test]
    fn a_limit_is_above_zero_and_within_its_cap() {
        assert!(Limits::new(1, 1).is_ok());
        assert!(Limits::new(MAX_FILE_BYTES, MAX_PROCESSES).is_ok());
        let bad = [
            (0, 1, "file size"),
            (MAX_FILE_BYTES + 1, 1, "file size"),
            (u64::MAX, 1, "file size"),
            (1, 0, "process"),
            (1, MAX_PROCESSES + 1, "process"),
            (1, u32::MAX, "process"),
        ];
        for (file_bytes, processes, which) in bad {
            let why = Limits::new(file_bytes, processes).unwrap_err();
            assert!(why.contains(which), "{file_bytes} {processes}: {why}");
        }
    }

    /// Catches: a field renamed, or a field the helper does not know taken for a limit.
    #[test]
    fn the_wire_names_are_fixed_and_unknown_fields_are_refused() {
        let limits = Limits::new(4096, 8).unwrap();
        let text = serde_json::to_string(&limits).unwrap();
        assert_eq!(text, r#"{"file_bytes":4096,"processes":8}"#);
        assert_eq!(serde_json::from_str::<Limits>(&text).unwrap(), limits);
        let extra = r#"{"file_bytes":4096,"processes":8,"cpu_seconds":1}"#;
        assert!(serde_json::from_str::<Limits>(extra).is_err());
    }
}
