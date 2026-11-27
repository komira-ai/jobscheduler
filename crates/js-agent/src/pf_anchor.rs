//! `__pf-anchor --flags-file <path>`: loads the pf anchor the agent's flags file names, unless it
//! is in force already. It is the program of the package's `js-agent.pf-pool` LaunchDaemon,
//! which runs it as root at load and every 30 seconds; the agent loads no anchor itself.
//!
//! The work is [`js_mac_provision::apply()`]: the file is trusted as the agent trusts it (a
//! regular file of root's or of the user running this, not writable by group or others), a file
//! that is not there or that sets no `--native-network=on` loads nothing, and the anchor, the
//! state directory and the `pfctl` are the file's. The verb prints one line to its output and
//! exits 0; a refused file or an anchor that could not be loaded is a line on its error
//! output and exit status 1; any other command line is a usage line and status 2.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use js_mac_provision::agent_flags::{self, FileFacts};
use js_mac_provision::pfctl::{Pfctl, SystemPfctl};

/// The verb, as the pf daemon's plist names it.
pub(crate) const VERB: &str = js_mac_provision::daemon::PF_ANCHOR_VERB;

/// The exit statuses.
pub(crate) mod status {
    /// The anchor is loaded, in force, or none is wanted.
    pub const OK: u8 = 0;
    /// The flags file was refused, or the anchor could not be loaded.
    pub const FAILED: u8 = 1;
    /// The command line is not `__pf-anchor --flags-file <path>`.
    pub const USAGE: u8 = 2;
}

/// Runs the verb of the command line `args` (the program name first, the verb second) on this
/// machine: the user running it may own the flags file, and `pfctl` is the real one.
pub(crate) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let euid = rustix::process::geteuid().as_raw();
    let pfctl_of = |path: &Path| -> Box<dyn Pfctl> { Box::new(SystemPfctl::new(path)) };
    ExitCode::from(run_over(
        args,
        euid,
        &agent_flags::read_system,
        &pfctl_of,
        out,
        err,
    ))
}

/// Runs the verb over `facts` (how the flags file is looked at) and `pfctl_of` (the runner of the
/// `pfctl` the file names), for `euid` as the user who may own the file. Returns its exit status.
pub(crate) fn run_over<'a>(
    args: &[OsString],
    euid: u32,
    facts: &dyn Fn(&Path) -> io::Result<FileFacts>,
    pfctl_of: &dyn Fn(&Path) -> Box<dyn Pfctl + 'a>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let [_, _, flag, file] = args else {
        return usage(err);
    };
    if flag.as_os_str() != "--flags-file" {
        return usage(err);
    }
    match js_mac_provision::apply(Path::new(file), euid, facts, pfctl_of) {
        Ok(applied) => {
            let _ = writeln!(out, "{applied}");
            status::OK
        }
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            status::FAILED
        }
    }
}

/// Writes the usage line to `err`; the exit status of a command line that is not the verb's.
fn usage(err: &mut dyn Write) -> u8 {
    let _ = writeln!(err, "usage: js-agent {VERB} --flags-file <path>");
    status::USAGE
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use js_testkit::dir::TestDir;

    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// A runner that cannot start `pfctl`, as on a host that has none.
    struct Missing;

    impl Pfctl for Missing {
        fn run(&self, _: &[&str]) -> io::Result<js_mac_provision::pfctl::Output> {
            Err(io::Error::new(io::ErrorKind::NotFound, "no pfctl here"))
        }
    }

    /// Writes a flags file as the agent's own: not writable by group or others, whatever the
    /// umask.
    fn write_flags(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Runs the verb on `file` over the real file facts as its owner, with a runner that has no
    /// `pfctl`. Returns the status, the output and the error output.
    fn over(file: &Path) -> (u8, String, String) {
        let args = words(&["js-agent", VERB, "--flags-file", file.to_str().unwrap()]);
        let euid = rustix::process::geteuid().as_raw();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let pfctl_of = |_: &Path| -> Box<dyn Pfctl> { Box::new(Missing) };
        let code = run_over(
            &args,
            euid,
            &agent_flags::read_system,
            &pfctl_of,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn a_flags_file_that_is_absent_or_not_on_prints_one_line_and_exits_0() {
        let dir = TestDir::under(std::env::temp_dir(), "pf-anchor-nothing");
        let absent = dir.join("absent");
        let (code, out, err) = over(&absent);
        assert_eq!(code, status::OK);
        assert_eq!(
            out,
            format!("no flags file {}: nothing to install\n", absent.display())
        );
        assert_eq!(err, "");
        let off = dir.join("off");
        write_flags(&off, "--native-network=loopback\n");
        let (code, out, err) = over(&off);
        assert_eq!(code, status::OK);
        assert_eq!(
            out,
            format!(
                "{}: --native-network=loopback: the agent checks no anchor, nothing to install\n",
                off.display()
            )
        );
        assert_eq!(err, "");
    }

    #[test]
    fn a_refused_flags_file_exits_1_and_names_the_flag_on_the_error_output() {
        let dir = TestDir::under(std::env::temp_dir(), "pf-anchor-refused");
        let file = dir.join("flags");
        write_flags(&file, "--native-network=on\n");
        let (code, out, err) = over(&file);
        assert_eq!(code, status::FAILED);
        assert_eq!(out, "");
        assert_eq!(
            err,
            format!(
                "js-agent: __pf-anchor: {}: --native-pf-uid-start: required with --native-network on\n",
                file.display()
            )
        );
    }

    #[test]
    fn an_anchor_that_cannot_be_loaded_exits_1_with_the_pfctl_error() {
        let dir = TestDir::under(std::env::temp_dir(), "pf-anchor-no-pfctl");
        let file = dir.join("flags");
        let state = dir.join("state");
        write_flags(
            &file,
            &format!(
                "--native-network=on\n--native-pf-uid-start=600\n--native-pf-uid-count=10\n\
                 --native-pf-state-dir={}\n",
                state.display()
            ),
        );
        let (code, out, err) = over(&file);
        assert_eq!(code, status::FAILED);
        assert_eq!(out, "");
        assert!(err.starts_with("js-agent: __pf-anchor: "), "{err}");
        assert!(err.contains("could not run pfctl"), "{err}");
        assert!(err.contains("no pfctl here"), "{err}");
    }

    #[test]
    fn any_other_command_line_is_a_usage_line_and_status_2() {
        let usage = "usage: js-agent __pf-anchor --flags-file <path>\n";
        for args in [
            &["js-agent", VERB][..],
            &["js-agent", VERB, "--flags-file"][..],
            &["js-agent", VERB, "--flags-file", "/f", "extra"][..],
            &["js-agent", VERB, "--flag-file", "/f"][..],
            &["js-agent", VERB, "/f", "--flags-file"][..],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let pfctl_of = |_: &Path| -> Box<dyn Pfctl> { Box::new(Missing) };
            let code = run_over(
                &words(args),
                0,
                &agent_flags::read_system,
                &pfctl_of,
                &mut out,
                &mut err,
            );
            assert_eq!(code, status::USAGE, "{args:?}");
            assert_eq!(String::from_utf8(err).unwrap(), usage, "{args:?}");
            assert!(out.is_empty(), "{args:?}");
        }
    }

    /// The verb on this machine: a file that is not there loads nothing and exits 0; a file that
    /// asks for the anchor with a `pfctl` that is not there exits 1.
    #[test]
    fn the_system_run_uses_the_real_file_facts_and_the_pfctl_the_file_names() {
        let dir = TestDir::under(std::env::temp_dir(), "pf-anchor-system");
        let absent = dir.join("absent");
        let args = words(&["js-agent", VERB, "--flags-file", absent.to_str().unwrap()]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args, &mut out, &mut err), ExitCode::SUCCESS);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("nothing to install")
        );
        assert!(err.is_empty());

        let file = dir.join("flags");
        write_flags(
            &file,
            &format!(
                "--native-network=on\n--native-pf-uid-start=600\n--native-pf-uid-count=10\n\
                 --native-pfctl=/nonexistent/pfctl\n--native-pf-state-dir={}\n",
                dir.join("state").display()
            ),
        );
        let args = words(&["js-agent", VERB, "--flags-file", file.to_str().unwrap()]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(&args, &mut out, &mut err),
            ExitCode::from(status::FAILED)
        );
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("could not run pfctl")
        );
    }
}
