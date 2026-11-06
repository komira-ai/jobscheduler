//! The archive of every platform: one zstd frame, the tar entries and their headers, and the same
//! bytes on every run.

use std::time::{Duration, UNIX_EPOCH};

use xtask::release::hostkeeper_package::{PackageArgs, file_name, run};

use crate::support::{
    ALL, FrameHeader, LINUX, Scratch, TEST_MIN, VERSION, binary, decode, file, frame_header,
    listing, package, version,
};

/// Catches a second frame or trailing bytes (hostkeeper refuses the package), a window above
/// hostkeeper's bound, a dictionary, or a frame with no content checksum.
#[test]
fn the_archive_is_one_zstd_frame_with_a_checksum_no_dictionary_and_a_window_of_at_most_8_mib() {
    for platform in ALL {
        let archive = package(platform);
        let header = frame_header(&archive);
        assert!(
            header.window <= hostkeeper_unpack::MAX_WINDOW_BYTES,
            "{platform}: window {}",
            header.window
        );
        assert_eq!(
            header,
            FrameHeader {
                single_segment: false,
                reserved: false,
                checksum: true,
                dictionary: false,
                window: header.window,
            },
            "{platform}"
        );
        let (tar, after) = decode(&archive);
        assert_eq!(after, 0, "{platform}: bytes after the one frame");
        assert_eq!(tar.len() % 512, 0, "{platform}: whole tar blocks");
        assert!(
            tar.ends_with(&[0; 1024]),
            "{platform}: the end-of-archive marker"
        );
    }
}

/// Catches another top-level name, an entry that is neither a file nor a directory, a directory
/// after its contents, an unsorted order, a mode other than 0755 (binary and directories) or 0644
/// (other files), a setuid bit, an owner, owner name or time taken from the build, a header that
/// is not ustar, and a file of one platform in the package of another.
#[test]
fn the_archive_holds_only_manifest_bin_files_and_units_each_directory_first_with_fixed_headers() {
    use tar::EntryType::{Directory, Regular};
    for platform in ALL {
        let listed = listing(&package(platform));
        let shape: Vec<(&str, tar::EntryType, u32)> = listed
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.mode))
            .collect();
        let expected: Vec<(&str, tar::EntryType, u32)> = match platform.as_str() {
            "macos-arm64" => vec![
                ("bin", Directory, 0o755),
                ("bin/js-agent", Regular, 0o755),
                ("manifest.toml", Regular, 0o644),
                ("units", Directory, 0o755),
                ("units/js-agent.plist", Regular, 0o644),
            ],
            _ => vec![
                ("bin", Directory, 0o755),
                ("bin/js-agent", Regular, 0o755),
                ("files", Directory, 0o755),
                ("files/tmpfiles.conf", Regular, 0o644),
                ("manifest.toml", Regular, 0o644),
                ("units", Directory, 0o755),
                ("units/js-agent-crun-gpu.service", Regular, 0o644),
                ("units/js-agent-crun.service", Regular, 0o644),
            ],
        };
        assert_eq!(shape, expected, "{platform}");
        let names: Vec<&str> = listed.iter().map(|entry| entry.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "{platform}: sorted by name");
        for entry in &listed {
            assert!(entry.ustar, "{}: a ustar header", entry.name);
            assert_eq!(
                entry.mode & 0o7000,
                0,
                "{}: no setuid, setgid or sticky bit",
                entry.name
            );
            assert_eq!(
                (entry.uid, entry.gid, entry.mtime),
                (0, 0, 0),
                "{}",
                entry.name
            );
            assert_eq!(
                entry.owner_names,
                (Vec::new(), Vec::new()),
                "{}",
                entry.name
            );
            if entry.kind == Directory {
                assert!(entry.content.is_empty(), "{}", entry.name);
            }
        }
    }
}

/// Catches a binary changed on the way in (re-signed, stripped, truncated): the signature the
/// macOS linker wrote would no longer match it.
#[test]
fn the_binary_in_every_package_is_the_input_byte_for_byte() {
    for platform in ALL {
        assert_eq!(
            file(&package(platform), "bin/js-agent"),
            binary(),
            "{platform}"
        );
    }
}

/// Catches anything of the build machine or the input file (its name, mode, owner or time, the
/// time of the run) reaching the package, so two builds of one commit would differ.
#[test]
fn two_runs_of_the_verb_give_the_same_bytes_whatever_the_input_files_name_mode_and_time() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("hostkeeper-package-twice");
    let inputs = [
        ("js-agent", 0o755, 0),
        ("another-name", 0o600, 1_800_000_000),
    ];
    let targets = [
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
        "aarch64-apple-darwin",
    ];
    // `Platform::ALL` is in the order of the spellings: linux-arm64, linux-x86_64, macos-arm64.
    for (platform, target) in ALL.into_iter().zip(targets) {
        let mut written = Vec::new();
        for (run_no, (name, mode, mtime)) in inputs.into_iter().enumerate() {
            let dir = scratch.path().join(format!("{platform}-{run_no}"));
            std::fs::create_dir_all(&dir).expect("dir");
            let bin = dir.join(name);
            std::fs::write(&bin, binary()).expect("binary");
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("mode");
            std::fs::File::options()
                .write(true)
                .open(&bin)
                .expect("open")
                .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
                .expect("time");
            let out = dir.join("out");
            run(&PackageArgs {
                binary: bin,
                target: target.to_owned(),
                version: VERSION.to_owned(),
                min_hostkeeper: TEST_MIN.to_owned(),
                out: out.clone(),
            })
            .expect("packaged");
            written.push(std::fs::read(out.join(file_name(version(), platform))).expect("package"));
        }
        assert_eq!(written[0], written[1], "{platform}");
        assert_eq!(written[0], package(platform), "{platform}");
    }
    // The two Linux packages differ: each manifest names its own platform.
    let [x86, arm] = LINUX.map(package);
    assert_ne!(x86, arm);
}
