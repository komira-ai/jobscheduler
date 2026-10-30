//! P-B4: a dylib chain and an executable linked with `@rpath` and `@loader_path` run from
//! the path they were built at, from a copy, and from the built tree after it is moved.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::Scratch;
use super::exec::{Outcome, describe, run};
use crate::parse::{dependents, rpaths};
use crate::report::{Probe, Report};

const RELOC_C: &str = "int reloc_value(void) { return 42; }\n";
const OUTER_C: &str =
    "int reloc_value(void);\nint outer_value(void) { return reloc_value() + 1; }\n";
const MAIN_C: &str = "#include <stdio.h>\nint outer_value(void);\nint main(void) { printf(\"%d\\n\", outer_value()); return 0; }\n";
const EXPECTED: &str = "43\n";

/// Where the trees ended up, for the probes that look at them afterwards.
pub struct Trees {
    /// The built tree, at its moved location.
    pub moved: PathBuf,
    /// The byte copy made while the original was still in place.
    pub copy: PathBuf,
}

fn clang(args: &[&OsStr]) -> Outcome {
    let mut cmd = Command::new("/usr/bin/clang");
    cmd.args(args);
    run(cmd)
}

/// Compiles `lib/libreloc.dylib`, `lib/libouter.dylib` and `bin/prog` under `tree`.
fn build(tree: &Path) -> Result<(), Outcome> {
    let (lib, bin, src) = (tree.join("lib"), tree.join("bin"), tree.join("src"));
    let (reloc_c, outer_c, main_c) = (src.join("reloc.c"), src.join("outer.c"), src.join("main.c"));
    let (libreloc, libouter, prog) = (
        lib.join("libreloc.dylib"),
        lib.join("libouter.dylib"),
        bin.join("prog"),
    );
    let made = fs::create_dir_all(&lib)
        .and_then(|()| fs::create_dir_all(&bin))
        .and_then(|()| fs::create_dir_all(&src));
    let wrote = made
        .and_then(|()| fs::write(&reloc_c, RELOC_C))
        .and_then(|()| fs::write(&outer_c, OUTER_C))
        .and_then(|()| fs::write(&main_c, MAIN_C));
    if let Err(err) = wrote {
        return Err(Outcome {
            code: None,
            ok: false,
            stdout: String::new(),
            stderr: format!("cannot write the sources: {err}"),
        });
    }
    let os = OsStr::new;
    let search = format!("-L{}", lib.display());
    let steps: [Vec<&OsStr>; 3] = [
        vec![
            os("-dynamiclib"),
            reloc_c.as_os_str(),
            os("-install_name"),
            os("@rpath/libreloc.dylib"),
            os("-o"),
            libreloc.as_os_str(),
        ],
        vec![
            os("-dynamiclib"),
            outer_c.as_os_str(),
            os(&search),
            os("-lreloc"),
            os("-install_name"),
            os("@rpath/libouter.dylib"),
            os("-Wl,-rpath,@loader_path"),
            os("-o"),
            libouter.as_os_str(),
        ],
        vec![
            main_c.as_os_str(),
            os(&search),
            os("-louter"),
            os("-Wl,-rpath,@loader_path/../lib"),
            os("-o"),
            prog.as_os_str(),
        ],
    ];
    for step in &steps {
        let out = clang(step);
        if !out.ok {
            return Err(out);
        }
    }
    Ok(())
}

/// Copies a directory tree of regular files (no extended attributes, no symlinks).
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn runs_from(report: &mut Report, name: &str, tree: &Path) {
    let outcome = run(Command::new(tree.join("bin/prog")));
    let held = outcome.ok && outcome.stdout == EXPECTED;
    let probe =
        Probe::asserted(&format!("relocate/{name}"), held).fact("stdout", outcome.stdout.trim());
    report.push(describe(probe, "", &outcome));
}

/// The install names and rpaths of the three Mach-O files, as `otool` prints them, with the
/// file's own name line left out so two locations compare.
fn readings(tree: &Path) -> String {
    let mut parts = Vec::new();
    for rel in ["bin/prog", "lib/libouter.dylib", "lib/libreloc.dylib"] {
        let file = tree.join(rel);
        let deps = run({
            let mut cmd = Command::new("/usr/bin/otool");
            cmd.arg("-L").arg(&file);
            cmd
        });
        let load = run({
            let mut cmd = Command::new("/usr/bin/otool");
            cmd.arg("-l").arg(&file);
            cmd
        });
        parts.push(format!(
            "{rel}: deps [{}] rpaths [{}]",
            dependents(&deps.stdout).join(", "),
            rpaths(&load.stdout).join(", ")
        ));
    }
    parts.join(" ; ")
}

pub fn probes(scratch: &Scratch, report: &mut Report) -> Option<Trees> {
    let built = scratch.base.join("reloc-a/tree");
    if let Err(out) = build(&built) {
        report.push(describe(Probe::asserted("relocate/build", false), "", &out));
        return None;
    }
    report.push(Probe::asserted("relocate/build", true));
    runs_from(report, "run-where-built", &built);

    let copy = scratch.base.join("reloc-b/deeper/still/tree");
    let copied = copy_tree(&built, &copy);
    report.push(Probe::asserted("relocate/copy", copied.is_ok()));
    if copied.is_ok() {
        runs_from(report, "run-from-copy", &copy);
    }

    let original = readings(&built);
    let moved = scratch.base.join("reloc-c/moved");
    let renamed =
        fs::create_dir_all(scratch.base.join("reloc-c")).and_then(|()| fs::rename(&built, &moved));
    report.push(Probe::asserted("relocate/move", renamed.is_ok()));
    if renamed.is_err() {
        return None;
    }
    runs_from(report, "run-after-move", &moved);
    report.push(Probe::asserted(
        "relocate/original-path-is-gone",
        !built.exists(),
    ));
    // Reported: the readings of the tree where it was built and where it is now. They name
    // only `@rpath`, `@loader_path` and system paths when nothing absolute was baked in.
    let after = readings(&moved);
    let absolute: Vec<&str> = original
        .split([',', ' ', '[', ']'])
        .filter(|t| t.starts_with('/') && !t.starts_with("/usr/lib") && !t.starts_with("/System"))
        .collect();
    report.push(
        Probe::info("relocate/otool-readings")
            .fact("where_built", original.clone())
            .fact("after_move", after.clone())
            .fact("readings_equal", original == after)
            .fact("absolute_non_system_names", absolute.join(" ")),
    );
    Some(Trees { moved, copy })
}
