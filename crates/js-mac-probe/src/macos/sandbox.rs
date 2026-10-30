//! `sandbox-exec` itself, and file writes under a deny profile.

use std::ffi::OsStr;
use std::process::Command;

use super::Scratch;
use super::exec::{Mode, Net, Outcome, describe, run};
use crate::report::{Probe, Report};

/// A path outside every grant that a control run may create and remove.
fn outside(scratch: &Scratch, name: &str) -> std::path::PathBuf {
    scratch
        .home
        .join(format!("js-mac-probe-{}-{name}", std::process::id()))
}

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let places = &scratch.places;
    // The simplest profile, from a harness that is not itself sandboxed.
    let allow_default = run({
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.args(["-p", "(version 1)(allow default)", "/usr/bin/true"]);
        cmd
    });
    report.push(describe(
        Probe::asserted("sandbox/allow-default", allow_default.ok),
        "",
        &allow_default,
    ));

    let off = Net::Off;
    let sandboxed = Mode::Sandboxed(&off, "");

    // The profile compiles and a write inside the run directory works: without this the
    // denials below prove nothing.
    let run_file = places.run.join("granted");
    let granted = places.child(&sandboxed, "write", run_file.as_os_str());
    report.push(describe(
        Probe::asserted("sandbox/write-in-run-dir", granted.ok && run_file.exists()),
        "",
        &granted,
    ));

    // Outside every grant: the control writes it, the profile refuses.
    let target = outside(scratch, "outside");
    let control = places.child(&Mode::Control, "write", target.as_os_str());
    let control_wrote = control.ok && target.exists();
    // A failed removal is reported by the next probe's absence check.
    let _ = std::fs::remove_file(&target);
    report.push(describe(
        Probe::asserted("sandbox/control-write-outside", control_wrote),
        "",
        &control,
    ));
    let denied = places.child(&sandboxed, "write", target.as_os_str());
    let denied_probe = Probe::asserted(
        "sandbox/deny-write-outside",
        denied.denied() && !target.exists(),
    );
    report.push(describe(denied_probe, "", &denied));
    let _ = std::fs::remove_file(&target);

    // Input protection comes after the run-directory grant and beats it (a later rule wins).
    // The control writes the same path first, so a refusal below is the profile's.
    let input_file = places.input.join("protected");
    let input_control = places.child(&Mode::Control, "write", input_file.as_os_str());
    let input_control_wrote = input_control.ok && input_file.exists();
    let _ = std::fs::remove_file(&input_file);
    report.push(describe(
        Probe::asserted("sandbox/control-write-input", input_control_wrote),
        "",
        &input_control,
    ));
    let input = places.child(&sandboxed, "write", input_file.as_os_str());
    let input_probe = Probe::asserted(
        "sandbox/later-deny-beats-earlier-allow",
        input.denied() && !input_file.exists(),
    );
    report.push(describe(input_probe, "", &input));

    // The shared temp folders the design leaves unwritable; the control writes the same path.
    let tmp =
        std::path::Path::new("/private/tmp").join(format!("js-mac-probe-{}", std::process::id()));
    let tmp_control = places.child(&Mode::Control, "write", tmp.as_os_str());
    let tmp_control_wrote = tmp_control.ok && tmp.exists();
    let _ = std::fs::remove_file(&tmp);
    report.push(describe(
        Probe::asserted("sandbox/control-write-private-tmp", tmp_control_wrote),
        "",
        &tmp_control,
    ));
    let tmp_denied = places.child(&sandboxed, "write", tmp.as_os_str());
    let tmp_probe = Probe::asserted(
        "sandbox/deny-write-private-tmp",
        tmp_denied.denied() && !tmp.exists(),
    );
    report.push(describe(tmp_probe, "", &tmp_denied));
    let _ = std::fs::remove_file(&tmp);

    // The uid's own /var/folders entry is writable (the toolchain's module cache lives there).
    let entry_file = places
        .uid_entry
        .join(format!("js-mac-probe-{}", std::process::id()));
    let entry = places.child(&sandboxed, "write", entry_file.as_os_str());
    let entry_probe = Probe::asserted(
        "sandbox/write-in-uid-entry",
        entry.ok && entry_file.exists(),
    )
    .fact("uid_entry", places.uid_entry.display());
    report.push(describe(entry_probe, "", &entry));
    let _ = std::fs::remove_file(&entry_file);

    // Reported: process visibility. `pgrep` (and `true`, to tell a refused exec from a refused
    // listing) under the design's process-info rule and two narrower ones.
    let variants: [(&str, &str); 4] = [
        ("none", ""),
        (
            "design",
            "(deny process-info* (target others))\n(allow process-info* (target self) (target same-sandbox))",
        ),
        (
            "pidinfo",
            "(deny process-info-pidinfo (target others))\n(allow process-info-pidinfo (target self))",
        ),
        ("listpids", "(deny process-info-listpids)"),
    ];
    // /bin/ps is setuid and was seen failing to start under a profile (see sandbox/ps-under-profile); pgrep lists by sysctl.
    let ps_args = [OsStr::new(".")];
    let count = |out: &Outcome| out.stdout.lines().count();
    let control = run(places.command(&Mode::Control, OsStr::new("/usr/bin/pgrep"), &ps_args));
    let mut probe =
        Probe::info("sandbox/process-info-others").fact("control_pgrep_processes", count(&control));
    for (name, rules) in variants {
        let mode = Mode::Sandboxed(&off, rules);
        let ps = run(places.command(&mode, OsStr::new("/usr/bin/pgrep"), &ps_args));
        let true_ = run(places.command(&mode, OsStr::new("/usr/bin/true"), &[]));
        probe = probe
            .fact(&format!("{name}_pgrep_processes"), count(&ps))
            .fact(&format!("{name}_true_ok"), true_.ok);
        probe = describe(probe, &format!("{name}_pgrep_"), &ps);
        if !true_.ok {
            probe = probe.fact(&format!("{name}_true_stderr"), true_.stderr_tail());
        }
    }
    report.push(probe);

    // Reported: /bin/ps itself, which is setuid. Observed once on a runner as a start failure
    // under a profile; recorded here so the finding has evidence in the report.
    let ps_control =
        run(places.command(&Mode::Control, OsStr::new("/bin/ps"), &[OsStr::new("-A")]));
    let ps_profile = run(places.command(&sandboxed, OsStr::new("/bin/ps"), &[OsStr::new("-A")]));
    let ps = Probe::info("sandbox/ps-under-profile")
        .fact("control_ps_lines", ps_control.stdout.lines().count())
        .fact("profile_ps_lines", ps_profile.stdout.lines().count());
    report.push(describe(
        describe(ps, "control_", &ps_control),
        "profile_",
        &ps_profile,
    ));
}
