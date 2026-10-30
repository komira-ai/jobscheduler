//! Is `DYLD_*` stripped when a process is started through a SIP-protected binary? Reported,
//! not asserted: a hosted runner may have SIP off, and the farm's answer is a different one.

use std::ffi::{OsStr, OsString};
use std::process::Command;

use super::Scratch;
use super::exec::{first_line, run};
use crate::report::{Probe, Report};

const VARIABLES: [&str; 2] = ["DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"];
const VALUE: &str = "/nonexistent/js-mac-probe";

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let exe = scratch.places.exe.clone().into_os_string();
    let mut probe = Probe::info("dyld/env-through-sip-binaries")
        .fact("bin_sh_flags", first_line("/bin/ls", &["-ldO", "/bin/sh"]))
        .fact(
            "usr_bin_env_flags",
            first_line("/bin/ls", &["-ldO", "/usr/bin/env"]),
        );
    for variable in VARIABLES {
        let child: Vec<OsString> = vec![exe.clone(), "child".into(), "env".into(), variable.into()];
        let script = format!("exec \"$0\" child env {variable}");
        let launches: [(&str, &str, Vec<OsString>); 4] = [
            ("direct", "", child[1..].to_vec()),
            ("usr_bin_env", "/usr/bin/env", child.clone()),
            (
                "bin_sh",
                "/bin/sh",
                vec!["-c".into(), script.into(), exe.clone()],
            ),
            ("usr_bin_xcrun", "/usr/bin/xcrun", child.clone()),
        ];
        for (name, launcher, args) in launches {
            let mut cmd = Command::new(if launcher.is_empty() {
                exe.as_os_str()
            } else {
                OsStr::new(launcher)
            });
            cmd.args(&args).env(variable, VALUE);
            let out = run(cmd);
            let seen = if out.ok {
                out.stdout.trim().to_owned()
            } else {
                format!("error: {}", out.stderr_tail())
            };
            probe = probe.fact(&format!("{name}_{}", variable.to_lowercase()), seen);
        }
    }
    report.push(probe);
}
