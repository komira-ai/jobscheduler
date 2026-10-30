//! A C and a Swift hello world and an ad-hoc `codesign`, under the profile and as a control.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use super::Scratch;
use super::exec::{Mode, Net, describe, run};
use crate::report::{Probe, Report};

const HELLO_C: &str = "#include <stdio.h>\nint main(void) { puts(\"hello\"); return 0; }\n";
const HELLO_SWIFT: &str = "print(\"hello\")\n";

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let off = Net::Off;
    // The control runs the same steps with no sandbox; if it fails, the sandboxed run
    // proves nothing and the run fails here.
    let control_dir = scratch.control.join("tool");
    let sandboxed_dir = scratch.places.run.join("tool");
    case(
        scratch,
        report,
        "control",
        &Mode::Control,
        &control_dir,
        true,
    );
    case(
        scratch,
        report,
        "sandboxed",
        &Mode::Sandboxed(&off, ""),
        &sandboxed_dir,
        true,
    );
}

/// The toolchain steps once. `asserted` false makes every step a reported fact (a trace run,
/// whose behaviour is not known beforehand).
pub fn case(
    scratch: &Scratch,
    report: &mut Report,
    label: &str,
    mode: &Mode<'_>,
    dir: &Path,
    asserted: bool,
) {
    let setup = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join("hello.c"), HELLO_C))
        .and_then(|()| fs::write(dir.join("hello.swift"), HELLO_SWIFT));
    if let Err(err) = setup {
        let name = format!("toolchain/{label}/setup");
        report.push(Probe::asserted(&name, false).fact("error", err));
        return;
    }
    let places = &scratch.places;
    let path = |name: &str| dir.join(name);
    let step =
        |report: &mut Report, name: &str, program: &OsStr, args: &[&OsStr], expect_hello: bool| {
            let outcome = run(places.command(mode, program, args));
            let hello = !expect_hello || outcome.stdout == "hello\n";
            let (full, held) = (format!("toolchain/{label}/{name}"), outcome.ok && hello);
            let probe = if asserted {
                Probe::asserted(&full, held)
            } else {
                Probe::info(&full).fact("held", held)
            };
            report.push(describe(probe, "", &outcome));
        };
    let (hello_c, hello_s) = (path("hello_c"), path("hello_s"));
    let module_cache = path("module-cache");
    step(
        report,
        "clang-build",
        OsStr::new("/usr/bin/clang"),
        &[
            path("hello.c").as_os_str(),
            OsStr::new("-o"),
            hello_c.as_os_str(),
        ],
        false,
    );
    step(report, "clang-run", hello_c.as_os_str(), &[], true);
    step(
        report,
        "codesign-adhoc",
        OsStr::new("/usr/bin/codesign"),
        &[
            OsStr::new("-s"),
            OsStr::new("-"),
            OsStr::new("--force"),
            hello_c.as_os_str(),
        ],
        false,
    );
    step(
        report,
        "codesign-verify",
        OsStr::new("/usr/bin/codesign"),
        &[OsStr::new("--verify"), hello_c.as_os_str()],
        false,
    );
    step(report, "signed-run", hello_c.as_os_str(), &[], true);
    step(
        report,
        "swiftc-build",
        OsStr::new("/usr/bin/swiftc"),
        &[
            path("hello.swift").as_os_str(),
            OsStr::new("-module-cache-path"),
            module_cache.as_os_str(),
            OsStr::new("-o"),
            hello_s.as_os_str(),
        ],
        false,
    );
    step(report, "swiftc-run", hello_s.as_os_str(), &[], true);
}
