//! The read allow-list of the design (`ReadModel::Allow`): the toolchain steps under it with
//! the no-sandbox control passing, the sandbox trace of the same steps (what a base set is
//! derived from), and a planted file that must stay unreadable, including when it is spelled
//! through `/System/Volumes/Data`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::Scratch;
use super::exec::{Custom, Mode, describe, first_line, run};
use super::toolchain::case;
use crate::parse::unlisted_reads;
use crate::report::{Probe, Report};

const SUBPATHS: [&str; 12] = [
    "/System/Library",
    "/System/Cryptexes",
    "/System/Volumes/Preboot/Cryptexes",
    "/usr/lib",
    "/usr/bin",
    "/usr/sbin",
    "/usr/libexec",
    "/usr/share",
    "/bin",
    "/sbin",
    "/private/var/db/dyld",
    "/private/var/db/timezone",
];
const LITERALS: [&str; 6] = [
    "/",
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];
const DATA: &str = "/System/Volumes/Data";
const SECRET: &[u8] = b"secret\n";

/// The profile of the design's `Allow` model for one run: reads denied then granted, writes
/// as the open model, network off, the floor last. `RUN`, `UIDENT`, `IN`, `XCODE_APP` and
/// `EXE` are parameters.
fn allow_profile(ancestors: bool) -> String {
    let metadata = if ancestors {
        "(allow file-read-metadata (path-ancestors (param \"RUN\")) (path-ancestors (param \"XCODE_APP\")) (path-ancestors (param \"EXE\")))"
    } else {
        "(allow file-read-metadata)"
    };
    let literals: Vec<String> = LITERALS
        .iter()
        .map(|p| format!("(literal \"{p}\")"))
        .collect();
    let subpaths: Vec<String> = SUBPATHS
        .iter()
        .map(|p| format!("(subpath \"{p}\")"))
        .collect();
    [
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        "(deny job-creation)".to_owned(),
        "(deny lsopen)".to_owned(),
        "(deny user-preference-write)".to_owned(),
        "(allow file-write* (subpath (param \"RUN\")) (subpath (param \"UIDENT\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))".to_owned(),
        "(deny file-read*)".to_owned(),
        metadata.to_owned(),
        format!("(allow file-read* {} {})", literals.join(" "), subpaths.join(" ")),
        "(allow file-read* (subpath (param \"RUN\")) (subpath (param \"UIDENT\")))".to_owned(),
        "(allow file-read* (subpath (param \"XCODE_APP\")) (literal (param \"EXE\")))".to_owned(),
        "(deny file-write* (subpath (param \"IN\")))".to_owned(),
        "(deny network*)".to_owned(),
        "(deny mach-register)".to_owned(),
        "(deny file-mount)".to_owned(),
    ]
    .join("\n")
}

/// The selected `Xcode.app` (or the command-line tools directory).
fn developer_app() -> PathBuf {
    let dir = first_line("/usr/bin/xcode-select", &["-p"]);
    let app = dir.strip_suffix("/Contents/Developer").unwrap_or(&dir);
    PathBuf::from(app)
}

fn read_child(scratch: &Scratch, mode: &Mode<'_>, path: &str) -> super::exec::Outcome {
    scratch
        .places
        .child(mode, "read", std::ffi::OsStr::new(path))
}

pub fn probes(scratch: &Scratch, trace_out: Option<&Path>, report: &mut Report) {
    let places = &scratch.places;
    let ancestors = run({
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.args([
            "-p",
            "(version 1)(allow default)(allow file-read-metadata (path-ancestors \"/usr\"))",
            "/usr/bin/true",
        ]);
        cmd
    });
    report.push(describe(
        Probe::info("sandbox/path-ancestors-filter").fact("compiles", ancestors.ok),
        "",
        &ancestors,
    ));

    let app = developer_app();
    let params = vec![
        ("XCODE_APP".to_owned(), app.clone()),
        ("EXE".to_owned(), places.exe.clone()),
    ];

    // The trace of the steps: what a base set is derived from. Which profile makes the
    // facility record on this macOS is not known, so each form runs `true` first and the
    // first that records is used for the steps. Reported, with the reads no grant above covers.
    let trace_file = trace_out.map_or_else(|| scratch.base.join("trace.sb"), Path::to_path_buf);
    let forms = [
        ("trace_only", "(version 1)\n(trace \"FILE\")"),
        (
            "deny_default",
            "(version 1)\n(deny default)\n(trace \"FILE\")",
        ),
        (
            "allow_default",
            "(version 1)\n(allow default)\n(trace \"FILE\")",
        ),
    ];
    let mut forms_probe = Probe::info("sandbox/trace-forms");
    let mut chosen = None;
    for (name, template) in forms {
        let file = scratch.base.join(format!("form-{name}.sb"));
        let custom = Custom {
            text: template.replace("FILE", &file.display().to_string()),
            params: Vec::new(),
        };
        let out = run(places.command(
            &Mode::Custom(&custom),
            std::ffi::OsStr::new("/usr/bin/true"),
            &[],
        ));
        let recorded = std::fs::metadata(&file).map_or(0, |m| m.len());
        forms_probe = describe(
            forms_probe.fact(&format!("{name}_recorded_bytes"), recorded),
            &format!("{name}_"),
            &out,
        );
        if chosen.is_none() && out.ok && recorded > 0 {
            chosen = Some(template);
        }
    }
    report.push(forms_probe);
    if let Some(template) = chosen {
        let trace = Custom {
            text: template.replace("FILE", &trace_file.display().to_string()),
            params: Vec::new(),
        };
        case(
            scratch,
            report,
            "trace",
            &Mode::Custom(&trace),
            &places.run.join("tool-trace"),
            false,
        );
    }
    let text = std::fs::read_to_string(&trace_file).unwrap_or_default();
    let mut subpaths: Vec<String> = SUBPATHS.iter().map(|s| (*s).to_owned()).collect();
    subpaths.extend([&places.run, &places.uid_entry, &app].map(|p| p.display().to_string()));
    let mut literals: Vec<String> = LITERALS.iter().map(|s| (*s).to_owned()).collect();
    literals.push(places.exe.display().to_string());
    let (sub, lit): (Vec<&str>, Vec<&str>) = (
        subpaths.iter().map(String::as_str).collect(),
        literals.iter().map(String::as_str).collect(),
    );
    let unlisted = unlisted_reads(&text, &sub, &lit);
    let head: Vec<&str> = text.lines().take(12).collect();
    report.push(
        Probe::info("sandbox/trace")
            .fact("trace_lines", text.lines().count())
            .fact("unlisted_read_count", unlisted.len())
            .fact(
                "unlisted_reads",
                unlisted
                    .iter()
                    .take(150)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | "),
            )
            .fact("head", head.join(" | ")),
    );

    // The toolchain under the allow-list; the control steps ran earlier and are asserted.
    let allow = Custom {
        text: allow_profile(ancestors.ok),
        params,
    };
    let mode = Mode::Custom(&allow);
    case(
        scratch,
        report,
        "allow",
        &mode,
        &places.run.join("tool-allow"),
        true,
    );

    // Reported: what the kernel logged as denied during the allow-list run, deduplicated.
    report.push(denials());

    // A planted file outside every grant, and one inside the run directory.
    let planted = scratch.control.join("planted-secret");
    let granted = places.run.join("planted-granted");
    let wrote = std::fs::write(&planted, SECRET).and_then(|()| std::fs::write(&granted, SECRET));
    if let Err(err) = wrote {
        report.push(Probe::asserted("allow/plant", false).fact("error", err));
        return;
    }
    let (planted, granted) = (planted.display().to_string(), granted.display().to_string());
    let (planted_data, granted_data) = (format!("{DATA}{planted}"), format!("{DATA}{granted}"));
    let secret = String::from_utf8_lossy(SECRET).into_owned();

    let control = read_child(scratch, &Mode::Control, &planted);
    let held = control.ok && control.stdout == secret;
    report.push(describe(
        Probe::asserted("allow/control-reads-planted", held),
        "",
        &control,
    ));
    let denied = read_child(scratch, &mode, &planted);
    report.push(describe(
        Probe::asserted("allow/denies-unlisted-read", denied.denied()),
        "",
        &denied,
    ));
    let inside = read_child(scratch, &mode, &granted);
    let held = inside.ok && inside.stdout == secret;
    report.push(describe(
        Probe::asserted("allow/reads-granted-tree", held),
        "",
        &inside,
    ));

    // Reported: the same files spelled through the Data volume's firmlink path.
    let control_data = read_child(scratch, &Mode::Control, &planted_data);
    let denied_data = read_child(scratch, &mode, &planted_data);
    let inside_data = read_child(scratch, &mode, &granted_data);
    let probe = Probe::info("allow/read-through-system-volumes-data")
        .fact("control_planted_ok", control_data.ok)
        .fact("allow_planted_ok", denied_data.ok)
        .fact("allow_granted_ok", inside_data.ok);
    let probe = describe(probe, "control_planted_", &control_data);
    let probe = describe(probe, "allow_planted_", &denied_data);
    report.push(describe(probe, "allow_granted_", &inside_data));
}

/// The sandbox denials in the unified log from the last minutes: `process operation path`.
fn denials() -> Probe {
    let out = run({
        let mut cmd = Command::new("/usr/bin/log");
        cmd.args([
            "show",
            "--last",
            "5m",
            "--style",
            "compact",
            "--predicate",
            "eventMessage CONTAINS \"deny(\"",
        ]);
        cmd
    });
    let mut seen = std::collections::BTreeSet::new();
    for line in out.stdout.lines() {
        let Some((_, rest)) = line.split_once("Sandbox: ") else {
            continue;
        };
        let Some((process, tail)) = rest.split_once('(') else {
            continue;
        };
        let Some((_, what)) = tail.split_once(") ") else {
            continue;
        };
        seen.insert(format!("{process} {what}"));
    }
    let listed: Vec<String> = seen.iter().take(200).cloned().collect();
    describe(
        Probe::info("sandbox/denials-in-log")
            .fact("count", seen.len())
            .fact("denials", listed.join(" | ")),
        "log_",
        &out,
    )
}
