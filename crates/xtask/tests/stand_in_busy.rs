//! A program the test rigs put in place for the code under test to execute
//! ([`crun_host::install_program`]) is never busy: a child that another thread of the test
//! binary forks while the program is being written, and that has not exec'd yet, holds no
//! descriptor of it, so the program starts.
//!
//! The source of the copy is a named pipe, so the write of the program waits, with its
//! destination open, until the test has forked: the interleaving that failed in CI (a
//! stand-in refused with "Text file busy") is the only one this test runs. The pipe is fed by
//! a process of its own, stopped until the fork is made, so the forked child holds no writer
//! of the pipe and the copy sees its end.
//!
//! Alone in its binary: the held child keeps a copy of every descriptor the process has open
//! while it is held, which would hold up another test waiting on a close.
#![cfg(target_os = "linux")]

mod crun_host;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use js_testkit::dir::TestDir;
use js_testkit::forked::ForkedChild;
use rustix::fs::{CWD, FileType, Mode, mknodat};
use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};

/// The feeder of the pipe `$1`: it opens the pipe, stops itself, and once continued writes
/// `$2` and ends, which closes the pipe.
const FEED: &str = r#"exec 3>"$1" && kill -STOP $$ && printf '%s' "$2" >&3"#;

/// The program put in place: it exits with a status of its own, so its run shows that the
/// whole program landed and started.
const PROGRAM: &[u8] = b"#!/bin/sh\nexit 7\n";

/// Asserts a stand-in whose write was under way when another thread forked a child (held
/// before its exec until the end of the test) starts and exits as written, while that child
/// is still held.
///
/// Catches a stand-in written by the test process itself (`std::fs::copy`), whose start then
/// fails with `ETXTBSY` ("Text file busy", the failure of the postinst tests in CI) for as long
/// as the child lives.
#[test]
fn a_program_being_installed_when_a_child_forks_is_not_busy() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stand-in-busy");
    let source = scratch.join("source");
    let to = scratch.join("stand-in");
    mknodat(CWD, &source, FileType::Fifo, Mode::from_raw_mode(0o600), 0).expect("a named pipe");
    let installer = {
        let (from, to): (PathBuf, PathBuf) = (source.clone(), to.clone());
        std::thread::spawn(move || crun_host::install_program(&from, &to))
    };
    let mut feeder = Command::new("sh")
        .args(["-c", FEED, "sh"])
        .arg(&source)
        .arg(std::str::from_utf8(PROGRAM).expect("text"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the feeder");
    let feeder_pid = Pid::from_child(&feeder);
    // The feeder stops only once its open of the pipe has returned, which is once the
    // installer has opened the pipe to read; the installer then opens the destination to
    // write, and keeps it open until the pipe ends.
    let (_, stop) = waitpid(Some(feeder_pid), WaitOptions::UNTRACED)
        .expect("waitpid")
        .expect("the feeder changed state");
    assert!(
        stop.stopped(),
        "the feeder ended before it stopped: {stop:?}"
    );
    while !to.exists() && !installer.is_finished() {
        std::thread::yield_now();
    }
    assert!(
        to.exists(),
        "the installer ended before it made the program"
    );
    let held = ForkedChild::before_exec();
    kill_process(feeder_pid, Signal::CONT).expect("the feeder goes on");
    assert!(feeder.wait().expect("the feeder ends").success());
    installer.join().expect("the program is installed");

    assert_eq!(std::fs::read(&to).expect("the program"), PROGRAM);
    let status = Command::new(&to)
        .status()
        .expect("the program starts while the forked child is held");
    assert_eq!(status.code(), Some(7));
    held.finish();
}
