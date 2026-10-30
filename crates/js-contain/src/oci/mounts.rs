//! What a running container's mounts really are, against what its bundle asked for.
//!
//! `crun` binds by path and applies the flags of a bind (`nosuid`, `nodev`, `ro`) in a
//! second step, so a bind could come up without them (a flag the kernel did not take, a
//! source that changed between the bundle and the start) and nothing would say so. The
//! engine this backend replaces read each container's mounts back after creating it and
//! refused a mismatch; [`problems`] is that read-back, over `/proc/<pid>/mountinfo` of the
//! container's first process ([`Oci::verify_mounts`](super::Oci::verify_mounts) reads it
//! after the start).
//!
//! Only binds and tmpfs mounts of the bundle are compared (the filesystems `crun` mounts
//! itself carry their flags in the bundle's options too, but a job cannot name their
//! source). For each, the flags `nosuid`, `nodev`, `noexec` and `ro` the bundle lists
//! must be in effect, and a mount the bundle does not make read-only must not be. A
//! mount point is looked up by its last entry, the one on top. A bound directory's own
//! submounts keep their own flags: only the mount the bundle names is checked.

use oci_spec::runtime::Spec;

/// What is wrong with the mounts of the container whose `/proc/<pid>/mountinfo` is
/// `mountinfo`, against the mounts of its bundle `config`: one line each, naming the
/// target. Empty when every mount the bundle asked for flags on has them.
#[must_use]
pub fn problems(config: &Spec, mountinfo: &str) -> Vec<String> {
    let live = live(mountinfo);
    let mut found = Vec::new();
    for mount in config.mounts().iter().flatten() {
        if !matches!(mount.typ().as_deref(), Some("bind" | "tmpfs")) {
            continue;
        }
        let target = mount.destination().to_string_lossy().into_owned();
        let asked = mount.options().as_deref().unwrap_or_default();
        let asks = |flag: &str| asked.iter().any(|option| option == flag);
        let Some((_, has)) = live.iter().rev().find(|(point, _)| *point == target) else {
            found.push(format!("{target}: not mounted"));
            continue;
        };
        for flag in FLAGS {
            if asks(flag) && !has.contains(&flag) {
                found.push(format!(
                    "{target}: {flag} was asked for and is not in effect (mounted {})",
                    has.join(",")
                ));
            }
        }
        if has.contains(&"ro") && !asks("ro") {
            found.push(format!(
                "{target}: mounted read-only, which was not asked for"
            ));
        }
    }
    found
}

/// The flags of a mount the bundle can ask for and `mountinfo` shows.
const FLAGS: [&str; 4] = ["nosuid", "nodev", "noexec", "ro"];

/// Each mount of `mountinfo`: its mount point, unescaped, and its per-mount options
/// (the sixth field of a line). A line too short to have them names no mount.
fn live(mountinfo: &str) -> Vec<(String, Vec<&str>)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ').skip(4);
            let point = unescape(fields.next()?);
            Some((point, fields.next()?.split(',').collect()))
        })
        .collect()
}

/// `field` with the octal escapes of `mountinfo` (`\040` for a space) decoded; a
/// backslash not followed by three octal digits of a byte stays as it is.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let decoded = (bytes[at] == b'\\')
            .then(|| bytes.get(at + 1..at + 4))
            .flatten()
            .filter(|digits| digits.iter().all(|d| (b'0'..=b'7').contains(d)))
            .and_then(|digits| {
                let value = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
                u8::try_from(value).ok()
            });
        if let Some(byte) = decoded {
            out.push(byte);
            at += 4;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A bundle with these mounts: `(destination, type, options)`.
    fn bundle(mounts: &[(&str, &str, &[&str])]) -> Spec {
        let mounts: Vec<_> = mounts
            .iter()
            .map(|(destination, typ, options)| {
                json!({"destination": destination, "type": typ, "source": "x", "options": options})
            })
            .collect();
        serde_json::from_value(json!({"ociVersion": "1.0.0", "mounts": mounts})).expect("spec")
    }

    /// One mountinfo line: mount point and the per-mount options.
    fn line(point: &str, options: &str) -> String {
        format!("41 30 8:1 /src {point} {options} master:1 - ext4 /dev/sda1 rw\n")
    }

    /// Catches a bind that came up with every flag it was asked for being reported: no
    /// problem, for a read-write and a read-only bind and a tmpfs.
    #[test]
    fn mounts_with_their_flags_are_fine() {
        let config = bundle(&[
            ("/scratch", "bind", &["rbind", "rw", "nosuid", "nodev"]),
            ("/run/js-files", "bind", &["rbind", "ro", "nosuid", "nodev"]),
            ("/tmp", "tmpfs", &["nosuid", "noexec", "nodev", "size=64m"]),
        ]);
        let info = [
            line("/scratch", "rw,nosuid,nodev,relatime"),
            line("/run/js-files", "ro,nosuid,nodev,relatime"),
            line("/tmp", "rw,nosuid,nodev,noexec,relatime"),
        ]
        .concat();
        assert_eq!(problems(&config, &info), Vec::<String>::new());
    }

    /// Catches a bind that lost `nosuid` or `nodev` (set-uid files or device nodes
    /// working on a `--mount-root`) going unreported: each missing flag is its own
    /// problem naming the target and the flag.
    #[test]
    fn a_bind_without_nosuid_or_nodev_is_reported() {
        let config = bundle(&[("/workspace", "bind", &["rbind", "rw", "nosuid", "nodev"])]);
        let both = problems(&config, &line("/workspace", "rw,relatime"));
        assert_eq!(both.len(), 2, "{both:?}");
        assert!(both[0].starts_with("/workspace: "), "{both:?}");
        assert!(both[0].contains("nosuid"), "{both:?}");
        assert!(both[1].starts_with("/workspace: "), "{both:?}");
        assert!(both[1].contains("nodev"), "{both:?}");
        let one = problems(&config, &line("/workspace", "rw,nosuid,relatime"));
        assert_eq!(one.len(), 1, "{one:?}");
        assert!(one[0].contains("nodev"), "{one:?}");
    }

    /// Catches a `/tmp` that came up able to run programs, and a read-only bind that is
    /// writable (the run's files), going unreported.
    #[test]
    fn noexec_and_read_only_are_checked_too() {
        let config = bundle(&[
            ("/tmp", "tmpfs", &["noexec", "nosuid", "nodev"]),
            ("/run/js-files", "bind", &["rbind", "ro", "nosuid", "nodev"]),
        ]);
        let info = [
            line("/tmp", "rw,nosuid,nodev,relatime"),
            line("/run/js-files", "rw,nosuid,nodev,relatime"),
        ]
        .concat();
        let found = problems(&config, &info);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("/tmp: "), "{found:?}");
        assert!(found[0].contains("noexec"), "{found:?}");
        assert!(found[1].starts_with("/run/js-files: "), "{found:?}");
        assert!(found[1].contains("ro"), "{found:?}");
    }

    /// Catches a read-write bind that is read-only (a job that cannot write its scratch)
    /// going unreported.
    #[test]
    fn a_writable_bind_that_is_read_only_is_reported() {
        let config = bundle(&[("/scratch", "bind", &["rbind", "rw", "nosuid", "nodev"])]);
        let found = problems(&config, &line("/scratch", "ro,nosuid,nodev,relatime"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("read-only"), "{found:?}");
    }

    /// Catches a mount that is not there at all going unreported, and a path that is
    /// only a prefix of a mount point (`/scratch` in `/scratch2`) taken for it.
    #[test]
    fn a_mount_that_is_missing_is_reported() {
        let config = bundle(&[("/scratch", "bind", &["rbind", "nosuid", "nodev"])]);
        let found = problems(&config, &line("/scratch2", "rw,nosuid,nodev"));
        assert_eq!(found, ["/scratch: not mounted"]);
        assert_eq!(problems(&config, ""), ["/scratch: not mounted"]);
    }

    /// Catches the wrong entry read when a mount point has been mounted over: the last
    /// entry is the one the job sees, so a good mount under a bad one is a problem, and a
    /// bad one under a good one is not.
    #[test]
    fn the_entry_on_top_is_the_one_checked() {
        let config = bundle(&[("/scratch", "bind", &["rbind", "nosuid", "nodev"])]);
        let bad_on_top = [
            line("/scratch", "rw,nosuid,nodev"),
            line("/scratch", "rw,relatime"),
        ]
        .concat();
        assert_eq!(problems(&config, &bad_on_top).len(), 2);
        let good_on_top = [
            line("/scratch", "rw,relatime"),
            line("/scratch", "rw,nosuid,nodev"),
        ]
        .concat();
        assert!(problems(&config, &good_on_top).is_empty());
    }

    /// Catches a mount point with a space in it (mountinfo writes `\040`, a tab `\011`, a
    /// backslash `\134`) not matched to the bundle's path.
    #[test]
    fn escaped_mount_points_are_read_back() {
        let config = bundle(&[("/my dir\\x", "bind", &["rbind", "nosuid", "nodev"])]);
        let escaped = line("/my\\040dir\\134x", "rw,nosuid,nodev");
        assert!(problems(&config, &escaped).is_empty());
        let tab = bundle(&[("/a\tb", "bind", &["nosuid"])]);
        assert!(problems(&tab, &line("/a\\011b", "rw,nosuid")).is_empty());
        // A backslash that starts no escape of a byte stays: too short, not octal, or
        // above 255.
        for odd in ["/x\\1", "/x\\9zz", "/x\\777"] {
            let config = bundle(&[(odd, "bind", &["nosuid"])]);
            assert!(
                problems(&config, &line(odd, "rw,nosuid")).is_empty(),
                "{odd}"
            );
        }
    }

    /// Catches filesystems `crun` mounts itself, mounts with no flags asked, and a
    /// short or empty line being compared or crashing the reader: only binds and tmpfs
    /// mounts are checked, and an entry too short to hold options names no mount.
    #[test]
    fn other_mounts_and_short_lines_are_ignored() {
        let config = bundle(&[
            ("/proc", "proc", &["nosuid"]),
            ("/job/root", "overlay", &["nosuid"]),
            ("/plain", "bind", &["rbind"]),
        ]);
        let info = format!("short line\n\n{}", line("/plain", "rw"));
        assert!(problems(&config, &info).is_empty());
        let config = serde_json::from_value::<Spec>(json!({"ociVersion": "1.0.0"})).expect("spec");
        assert!(problems(&config, &info).is_empty());
    }

    /// Catches a mount with no options at all, and one with no type, being treated as
    /// flagged: neither asks for a flag.
    #[test]
    fn a_mount_without_options_or_type_asks_for_nothing() {
        let config: Spec = serde_json::from_value(json!({"ociVersion": "1.0.0", "mounts": [
            {"destination": "/a", "type": "bind", "source": "x"},
            {"destination": "/b", "source": "x", "options": ["nosuid"]},
        ]}))
        .expect("spec");
        assert!(problems(&config, &line("/a", "rw")).is_empty());
    }
}
