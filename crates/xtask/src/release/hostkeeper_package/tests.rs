//! The parts of the package verb that need no files: the target table, the manifest's
//! placeholders and the writer's failures.

use std::io::{self, Write};

use super::*;

/// A sink that takes `budget` bytes and then refuses every write.
#[derive(Debug)]
struct Budget {
    budget: usize,
}

impl Write for Budget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.budget == 0 {
            return Err(io::Error::other("the sink is full"));
        }
        let taken = buf.len().min(self.budget);
        self.budget -= taken;
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// Catches a target table that maps a build to the wrong platform, or accepts a target no
/// package is made for.
#[test]
fn each_rust_target_maps_to_its_platform_and_any_other_is_refused() {
    assert_eq!(
        platform_of("x86_64-unknown-linux-musl"),
        Ok(Platform::LinuxX86_64)
    );
    assert_eq!(
        platform_of("aarch64-unknown-linux-musl"),
        Ok(Platform::LinuxArm64)
    );
    assert_eq!(
        platform_of("aarch64-apple-darwin"),
        Ok(Platform::MacosArm64)
    );
    for bad in [
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "",
        "linux-arm64",
    ] {
        assert_eq!(platform_of(bad), Err(Error::UnknownTarget(bad.to_owned())));
    }
}

/// Catches a manifest template with a placeholder the verb does not fill (hostkeeper would
/// read `@VERSION@` as the version), a filled value that is not in the manifest, or a
/// template that does not hold a placeholder once.
#[test]
fn the_manifest_has_every_placeholder_once_and_none_is_left_after_filling_it() {
    for template in [LINUX_MANIFEST, MACOS_MANIFEST] {
        for placeholder in PLACEHOLDERS {
            let lines = template
                .lines()
                .filter(|line| !line.starts_with('#') && line.contains(placeholder))
                .count();
            assert_eq!(lines, 1, "{placeholder}");
        }
    }
    for platform in Platform::ALL {
        let text = manifest(platform, v(1, 22, 3), MIN_HOSTKEEPER);
        assert!(!text.contains('@'), "{platform}: {text}");
        assert!(text.contains("version        = \"1.22.3\""), "{platform}");
        assert!(
            text.contains(&format!("platform       = \"{}\"", platform.as_str())),
            "{platform}"
        );
        assert!(
            text.contains(&format!("min_hostkeeper = \"{MIN_HOSTKEEPER}\"")),
            "{platform}"
        );
    }
}

/// Catches an order in which a directory follows a file of it (hostkeeper's unpacker needs
/// the directory first), a Mac package that holds Linux files, and a Linux package that
/// holds a plist.
#[test]
fn the_entries_are_sorted_with_each_directory_before_its_files() {
    let names = |platform| -> Vec<String> {
        entries(platform, v(1, 2, 3), MIN_HOSTKEEPER, b"binary")
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    };
    assert_eq!(
        names(Platform::LinuxX86_64),
        [
            "bin",
            "bin/js-agent",
            "files",
            "files/tmpfiles.conf",
            "manifest.toml",
            "units",
            "units/js-agent-crun-gpu.service",
            "units/js-agent-crun.service",
        ]
    );
    assert_eq!(names(Platform::LinuxArm64), names(Platform::LinuxX86_64));
    assert_eq!(
        names(Platform::MacosArm64),
        [
            "bin",
            "bin/js-agent",
            "manifest.toml",
            "units",
            "units/js-agent.plist"
        ]
    );
}

/// Catches a writer that swallows a sink's failure: the package would be cut short and still
/// be reported as written.
#[test]
fn a_sink_that_fails_is_an_error_naming_the_entry_or_the_end() {
    let list = entries(Platform::MacosArm64, v(1, 2, 3), MIN_HOSTKEEPER, b"binary");
    let first = tar_to(Budget { budget: 0 }, &list).unwrap_err();
    assert!(
        matches!(&first, Error::Archive { what, why }
            if what == "the entry bin" && why.contains("the sink is full")),
        "{first:?}"
    );
    let whole = tar_to(Vec::new(), &list).expect("a tar").len();
    let end = tar_to(
        Budget {
            budget: whole - 1024,
        },
        &list,
    )
    .unwrap_err();
    assert!(
        matches!(&end, Error::Archive { what, .. } if what == "the end of the archive"),
        "{end:?}"
    );
}

/// Catches a package name that does not follow hostkeeper's `{service}-{version}-{platform}`.
#[test]
fn the_package_file_is_named_for_the_version_and_the_platform() {
    assert_eq!(
        file_name(v(1, 22, 3), Platform::LinuxArm64),
        "js-agent-1.22.3-linux-arm64.tar.zst"
    );
}
