//! The FUSE probe over fake files and fake device nodes.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use js_contain::container::DeviceNode;

use super::{FuseEnv, probe};
use crate::crun::NodeError;

/// The files of a fake host: a path and the text it holds; a path not listed is not there.
fn files(listed: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, String> + use<> {
    let held: BTreeMap<String, String> = listed
        .iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect();
    move |path| {
        held.get(path)
            .cloned()
            .ok_or_else(|| format!("{path}: No such file or directory (os error 2)"))
    }
}

fn node_of(path: &Path, major: u32, minor: u32) -> Result<DeviceNode, NodeError> {
    Ok(DeviceNode {
        path: path.to_string_lossy().into_owned(),
        major,
        minor,
        uid: 0,
        gid: 0,
        mode: 0o666,
    })
}

/// The FUSE device, 10:229.
fn fuse(path: &Path) -> Result<DeviceNode, NodeError> {
    node_of(path, 10, 229)
}

/// Another character device, 1:3 (`/dev/null`).
fn null(path: &Path) -> Result<DeviceNode, NodeError> {
    node_of(path, 1, 3)
}

/// No node at the path.
fn gone(path: &Path) -> Result<DeviceNode, NodeError> {
    Err(NodeError::Unreadable {
        path: path.to_owned(),
        error: io::Error::from(io::ErrorKind::NotFound),
    })
}

/// Something that is not a character device at the path.
fn plain(path: &Path) -> Result<DeviceNode, NodeError> {
    Err(NodeError::NotCharacterDevice {
        path: path.to_owned(),
    })
}

fn release(text: &str) -> impl Fn(&str) -> Result<String, String> + use<> {
    files(&[("/proc/sys/kernel/osrelease", text)])
}

/// Asserts that kernels from 6.14 on pass the version check, whatever follows the minor
/// number (a patch level, a build suffix, a newline), and that 6.13 and older are refused
/// with the kernel's version and the floor in the sentence. Catches a comparison that reads
/// 7.0 as below 6.14 (the minor alone), and one that lets 6.8 through.
#[test]
fn the_kernel_must_be_at_least_6_14() {
    for text in [
        "6.14.0\n",
        "6.14",
        "6.15.2-1-default",
        "7.0.0-42-generic\n",
        "10.1.0",
    ] {
        assert_eq!(probe(&release(text), fuse).jobs, Ok(()), "{text:?}");
    }
    for (text, shown) in [
        ("6.13.9\n", "6.13"),
        ("6.8.0-1017-azure\n", "6.8"),
        ("5.15.0-100-generic", "5.15"),
    ] {
        assert_eq!(
            probe(&release(text), fuse).jobs,
            Err(format!("the kernel is {shown}, below 6.14")),
            "{text:?}"
        );
    }
}

/// Asserts that a release that does not start with two numbers around a dot, and a release
/// file that cannot be read, are refused with the text or the reason in the sentence. Catches
/// a probe that treats a kernel it cannot read as one that fits.
#[test]
fn a_kernel_version_that_cannot_be_read_is_refused() {
    for text in ["garbage\n", "x.14.0", "6.x", "6."] {
        let want = format!(
            "the kernel's version {:?} does not start with <major>.<minor>",
            text.trim()
        );
        assert_eq!(probe(&release(text), fuse).jobs, Err(want), "{text:?}");
    }
    assert_eq!(
        probe(&files(&[]), fuse).jobs,
        Err(
            "the kernel's version cannot be read: /proc/sys/kernel/osrelease: No such file or \
             directory (os error 2)"
                .to_owned()
        )
    );
}

/// Asserts that a missing /dev/fuse, a path that is not a character device, and a character
/// device with other numbers are each refused with their own sentence, and that the
/// uring answer is the same refusal. Catches a host reported as offering the device when the
/// node is not the FUSE device.
#[test]
fn dev_fuse_must_be_the_fuse_device() {
    let read = release("6.14.0");
    let missing = probe(&read, gone);
    assert_eq!(missing.jobs, Err("/dev/fuse: entity not found".to_owned()));
    assert_eq!(missing.uring, missing.jobs);
    assert_eq!(
        probe(&read, plain).jobs,
        Err("/dev/fuse is not a character device".to_owned())
    );
    assert_eq!(
        probe(&read, null).jobs,
        Err("/dev/fuse is not the FUSE device (char 10:229)".to_owned())
    );
}

/// Asserts that the fuse module's enable_uring is read as on for `Y` and `1`, and as off for
/// `N`, `0` and a file that is not there, with the exact sentence for off. Catches
/// `fuse-uring` reported on a kernel that does not run the FUSE queue over io_uring.
#[test]
fn enable_uring_is_on_for_y_or_1_and_off_otherwise() {
    let with = |text: Option<&str>| {
        let mut listed = vec![("/proc/sys/kernel/osrelease", "6.14.0")];
        if let Some(text) = text {
            listed.push(("/sys/module/fuse/parameters/enable_uring", text));
        }
        probe(&files(&listed), fuse)
    };
    for text in ["Y\n", "1\n", "y"] {
        let env = with(Some(text));
        assert_eq!(env.jobs, Ok(()));
        assert_eq!(env.uring, Ok(()), "{text:?}");
    }
    for text in [Some("N\n"), Some("0\n"), None] {
        let env = with(text);
        assert_eq!(env.jobs, Ok(()));
        assert_eq!(
            env.uring,
            Err("the fuse module's enable_uring is off".to_owned()),
            "{text:?}"
        );
    }
}

/// Asserts that io_uring_disabled 0 and a missing sysctl leave FUSE over io_uring usable, and
/// that 1 and 2 do not, naming the value; and that the device itself is still offered.
/// Catches `fuse-uring` reported where io_uring is turned off for the host.
#[test]
fn io_uring_disabled_must_be_0_or_absent() {
    let with = |text: Option<&str>| {
        let mut listed = vec![
            ("/proc/sys/kernel/osrelease", "6.14.0"),
            ("/sys/module/fuse/parameters/enable_uring", "Y"),
        ];
        if let Some(text) = text {
            listed.push(("/proc/sys/kernel/io_uring_disabled", text));
        }
        probe(&files(&listed), fuse)
    };
    for text in [Some("0\n"), None] {
        assert_eq!(
            with(text),
            FuseEnv {
                jobs: Ok(()),
                uring: Ok(())
            },
            "{text:?}"
        );
    }
    for value in ["1", "2"] {
        let text = format!("{value}\n");
        let env = with(Some(text.as_str()));
        assert_eq!(env.jobs, Ok(()));
        assert_eq!(
            env.uring,
            Err(format!("kernel.io_uring_disabled is {value}"))
        );
    }
}
