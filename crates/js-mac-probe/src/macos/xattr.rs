//! P-B5: extended attributes (`com.apple.provenance`, `com.apple.quarantine`) on files the
//! compiler wrote, files Rust copied, and files `tar` extracted. Reported only: how a recent
//! macOS treats these is not known from here.

use std::path::Path;
use std::process::Command;

use super::Scratch;
use super::exec::{Outcome, run};
use super::relocate::Trees;
use crate::report::{Probe, Report};

fn listing(dir: &Path) -> Vec<String> {
    let out = run({
        let mut cmd = Command::new("/usr/bin/xattr");
        cmd.arg("-lr").arg(dir);
        cmd
    });
    let text = if out.ok { out.stdout } else { out.stderr };
    text.lines().map(str::to_owned).collect()
}

fn note(probe: Probe, name: &str, dir: &Path) -> Probe {
    let rows = listing(dir);
    let joined: String = rows.join(" | ").chars().take(600).collect();
    probe
        .fact(&format!("{name}_xattr_rows"), rows.len())
        .fact(
            &format!("{name}_has_provenance"),
            rows.iter().any(|r| r.contains("com.apple.provenance")),
        )
        .fact(
            &format!("{name}_has_quarantine"),
            rows.iter().any(|r| r.contains("com.apple.quarantine")),
        )
        .fact(&format!("{name}_xattrs"), joined)
}

fn tar(args: &[&std::ffi::OsStr]) -> Outcome {
    let mut cmd = Command::new("/usr/bin/tar");
    cmd.args(args);
    run(cmd)
}

pub fn probes(scratch: &Scratch, trees: Option<&Trees>, report: &mut Report) {
    let Some(trees) = trees else {
        report.push(Probe::info("xattr/trees").fact("skipped", "no tree was built"));
        return;
    };
    let os = std::ffi::OsStr::new;
    let archive = scratch.base.join("tree.tar");
    let parent = trees.copy.parent().unwrap_or(&trees.copy);
    let name = trees.copy.file_name().unwrap_or_default();
    let made = tar(&[
        os("-cf"),
        archive.as_os_str(),
        os("-C"),
        parent.as_os_str(),
        name,
    ]);
    let mut probe = Probe::info("xattr/trees").fact("tar_create_ok", made.ok);
    probe = note(probe, "clang_built", &trees.moved);
    probe = note(probe, "rust_copied", &trees.copy);
    for (label, flags) in [
        ("tar_extracted", None),
        ("tar_no_xattrs", Some("--no-xattrs")),
    ] {
        let dest = scratch.base.join(format!("extract-{label}"));
        let extracted = std::fs::create_dir_all(&dest).is_ok() && {
            let mut args = vec![os("-xf"), archive.as_os_str(), os("-C"), dest.as_os_str()];
            args.extend(flags.map(os));
            tar(&args).ok
        };
        probe = probe.fact(&format!("{label}_ok"), extracted);
        probe = note(probe, label, &dest);
        let prog = run(Command::new(dest.join(name).join("bin/prog")));
        probe = probe.fact(&format!("{label}_prog_runs"), prog.ok);
    }
    report.push(probe);
}
