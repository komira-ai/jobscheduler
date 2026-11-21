//! `cargo xtask mac-root-tests`'s real machine and its verb: `xtask-fake` as cargo (printing
//! the JSON messages a test writes for it) and as sudo (recording what it was asked to run),
//! and the real `xtask` program, which refuses before anything runs as root.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::dir::TestDir;
use xtask::mac_root::{Machine as _, RealMachine};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");

/// One test's directories: the repository root cargo runs in (with the messages the fake
/// cargo prints) and the fakes' directory, `cargo` and `sudo` in it.
struct Place {
    dir: TestDir,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("mac-root-{name}"));
        std::fs::create_dir_all(dir.join("root")).expect("root");
        std::fs::create_dir_all(dir.join("bin")).expect("bin");
        for name in ["cargo", "sudo"] {
            std::fs::hard_link(FAKE, dir.join("bin").join(name)).expect("link the fake");
        }
        std::fs::write(
            dir.join("root/cargo-messages"),
            "{\"reason\":\"build-finished\"}\n",
        )
        .expect("messages");
        Place { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.dir.join("bin").join(name)
    }

    /// The failure the fakes are asked for.
    fn defect(&self, defect: &str) {
        std::fs::write(self.bin("defect"), defect).expect("defect");
    }

    fn read(&self, path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }
}

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// Catches a build run anywhere but the repository root, with flags other than the JSON
/// messages of a locked build of exactly the targets asked for, or a failed build passed
/// over.
#[test]
fn the_real_machine_builds_with_cargo_in_the_root() {
    let place = Place::new("build");
    let (cargo, root, sudo) = (place.bin("cargo"), place.root(), place.bin("sudo"));
    let machine = RealMachine {
        cargo: cargo.as_os_str(),
        root: &root,
        sudo: &sudo,
    };
    let messages = "{\"reason\":\"build-finished\"}\n";
    assert_eq!(
        machine.build("js-agent", &["a", "b"]),
        Ok(messages.to_owned())
    );
    assert_eq!(
        machine.build_bin("js-mac-provision", "js-mac-provision"),
        Ok(messages.to_owned())
    );
    assert_eq!(
        place.read(root.join("target/debug/cargo-args")),
        "test --locked --no-run --message-format=json -p js-agent --test a --test b\n\
         build --locked --message-format=json -p js-mac-provision --bin js-mac-provision\n"
    );
    place.defect("build-fails");
    assert_eq!(
        machine
            .build_bin("js-mac-provision", "js-mac-provision")
            .map_err(|e| e.to_string()),
        Err(
            "cargo build -p js-mac-provision --bin js-mac-provision failed: exit status: 101"
                .to_owned()
        )
    );
    let gone = place.bin("no-cargo");
    let machine = RealMachine {
        cargo: gone.as_os_str(),
        ..machine
    };
    let e = machine
        .build_bin("p", "p")
        .expect_err("no cargo")
        .to_string();
    assert!(e.starts_with("cargo: "), "{e}");
}

/// Catches a root command run without `-n` (a prompt that hangs a run), from the wrong
/// directory, with its output not in the log, or a failed or unstartable sudo passed over;
/// and a user program whose failure is read as an answer.
#[test]
fn the_real_machine_runs_root_commands_through_sudo_n_and_logs_the_binaries() {
    let place = Place::new("sudo");
    let (cargo, root, sudo) = (place.bin("cargo"), place.root(), place.bin("sudo"));
    let machine = RealMachine {
        cargo: cargo.as_os_str(),
        root: &root,
        sudo: &sudo,
    };
    let here = std::env::current_dir().expect("cwd");
    assert!(
        machine
            .root(&args(&["/bin/mkdir", "x"]))
            .expect("ran")
            .success()
    );
    let log = root.join("t.log");
    let (status, text) = machine
        .root_logged(&args(&["/usr/bin/env", "K=V", "/w/bin/t"]), &root, &log)
        .expect("ran");
    assert!(status.success());
    assert_eq!(text, "");
    assert!(log.is_file());
    assert_eq!(
        machine.read(sudo.to_str().expect("utf-8"), &["hello"]),
        Ok(String::new())
    );
    assert_eq!(
        place.read(place.bin("sudo-args")),
        format!(
            "{}: -n /bin/mkdir x\n{}: -n /usr/bin/env K=V /w/bin/t\n{}: hello\n",
            here.display(),
            root.display(),
            here.display()
        )
    );

    place.defect("fails:x,hello");
    assert!(
        !machine
            .root(&args(&["/bin/mkdir", "x"]))
            .expect("ran")
            .success()
    );
    assert_eq!(
        machine
            .read(sudo.to_str().expect("utf-8"), &["hello"])
            .map_err(|e| e.to_string()),
        Err(format!("{} hello failed: exit status: 101", sudo.display()))
    );

    let missing = root.join("no-dir/t.log");
    let e = machine
        .root_logged(&args(&["x"]), &root, &missing)
        .expect_err("no log directory")
        .to_string();
    assert!(e.starts_with(&format!("{}: ", missing.display())), "{e}");

    let gone = place.bin("no-sudo");
    let machine = RealMachine {
        sudo: &gone,
        ..machine
    };
    let prefix = format!("{}: ", gone.display());
    let e = machine
        .root(&args(&["x"]))
        .expect_err("no sudo")
        .to_string();
    assert!(e.starts_with(&prefix), "{e}");
    let e = machine
        .root_logged(&args(&["x"]), &root, &log)
        .expect_err("no sudo")
        .to_string();
    assert!(e.starts_with(&prefix), "{e}");
    let e = machine
        .read(gone.to_str().expect("utf-8"), &[])
        .expect_err("gone")
        .to_string();
    assert!(e.starts_with(&prefix), "{e}");
}

/// Catches the verb running anything before its checks pass: off a Mac it refuses, and on a
/// Mac a `--work` whose parent does not exist is refused, in both cases with nothing built and
/// nothing run as root; and a run without `--work` is a usage error.
#[test]
fn the_verb_refuses_before_it_builds_or_runs_anything() {
    let place = Place::new("verb");
    let work = place.dir.join("missing/work");
    let out = Command::new(XTASK)
        .arg("mac-root-tests")
        .arg("--work")
        .arg(&work)
        .arg("--root")
        .arg(place.root())
        .arg("--sudo")
        .arg(place.bin("sudo"))
        .env("CARGO", place.bin("cargo"))
        .output()
        .expect("run xtask");
    let expected = match cfg!(target_os = "macos") {
        true => format!(
            "xtask: --work {}: its parent is not a directory\n",
            work.display()
        ),
        false => format!(
            "xtask: mac-root-tests runs on a Mac; this is {}\n",
            std::env::consts::OS
        ),
    };
    assert_eq!(String::from_utf8_lossy(&out.stderr), expected);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        !Path::new(&place.bin("sudo-args")).exists(),
        "nothing ran as root"
    );
    assert!(
        !place.root().join("target/debug/cargo-args").exists(),
        "nothing was built"
    );

    let usage = Command::new(XTASK)
        .arg("mac-root-tests")
        .output()
        .expect("run xtask");
    assert_eq!(usage.status.code(), Some(2));
}
