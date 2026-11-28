//! The hidden verbs [`crate::main_with`] answers first, before the signal handlers are
//! installed and before any thread starts: each is a short-lived program of its own that runs
//! no agent.
//!
//! - `__supervise-native`: the start of one native Mac job in a process of its own, with one
//!   thread, which opens the job's audit session, drops to its slot user, sets its limits and
//!   applies its profile, then starts it and records how it went
//!   ([`crate::mac_supervisor`]). The native runtime starts it for every native job and for
//!   its self-test ([`crate::native`]).
//! - `image-pull` (Linux): pulls an image into the image store ([`crate::image::pull`]).
//! - `__pf-anchor`: loads the pf anchor the agent's flags file names, unless it is in force
//!   already. The package's `js-agent.pf-pool` LaunchDaemon runs it as root at load and every
//!   30 seconds ([`pf_anchor`]).
//! - `__account-cycle`: a development verb that runs the life cycle of one account of the
//!   Mac account pool once, to prove it on a Mac ([`crate::mac_accounts::cycle`]). It runs no
//!   job, and is removed once the native runtime proves an account's life cycle itself.

mod pf_anchor;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

/// Answers the hidden verb `args` names (the program name first, the verb second): its exit
/// status, or `None` when `args` names none of them and the caller goes on.
pub(crate) fn dispatch(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<ExitCode> {
    let verb = args.get(1).and_then(|verb| verb.to_str())?;
    if verb == crate::mac_supervisor::VERB {
        return Some(crate::mac_supervisor::run(args, out, err));
    }
    #[cfg(target_os = "linux")]
    if verb == crate::image::pull::VERB {
        let host = crate::image::pull::Host::system();
        return Some(crate::image::pull::run(args, out, err, &host));
    }
    if verb == pf_anchor::VERB {
        return Some(pf_anchor::run(args, out, err));
    }
    if verb == crate::mac_accounts::cycle::VERB {
        return Some(crate::mac_accounts::cycle::run(args, out, err));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// Asserts that `__account-cycle` is answered here, with its own usage error for a
    /// command line it cannot run, and that a verb that is not hidden, an unknown one and a
    /// command line with no verb are left to the caller, with nothing written.
    #[test]
    fn the_account_cycle_is_answered_and_other_verbs_are_not() {
        use crate::mac_accounts::cycle::{VERB, status};

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = dispatch(&words(&["js-agent", VERB]), &mut out, &mut err);
        assert_eq!(code, Some(ExitCode::from(status::USAGE)));
        assert!(
            String::from_utf8(err)
                .unwrap()
                .starts_with(&format!("js-agent: {VERB}: --state is required\n"))
        );
        assert!(out.is_empty());

        for args in [
            &["js-agent", "__no-such-verb"][..],
            &["js-agent", "--state-dir", "/s"][..],
            &["js-agent"][..],
            &[][..],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(dispatch(&words(args), &mut out, &mut err), None, "{args:?}");
            assert_eq!((out, err), (Vec::new(), Vec::new()), "{args:?}");
        }
    }

    /// Asserts that `__pf-anchor` is answered here: a command line with no flags file is a usage
    /// error (status 2, a usage line on the error output), and a flags file that is not there
    /// loads nothing and exits 0 with one line on the output.
    #[test]
    fn the_pf_anchor_is_answered_here() {
        use super::pf_anchor::{VERB, status};

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = dispatch(&words(&["js-agent", VERB]), &mut out, &mut err);
        assert_eq!(code, Some(ExitCode::from(status::USAGE)));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "usage: js-agent __pf-anchor --flags-file <path>\n"
        );
        assert!(out.is_empty());

        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "hidden-pf-anchor");
        let absent = dir.join("flags");
        let args = words(&["js-agent", VERB, "--flags-file", absent.to_str().unwrap()]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            dispatch(&args, &mut out, &mut err),
            Some(ExitCode::from(status::OK))
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("no flags file {}: nothing to install\n", absent.display())
        );
        assert!(err.is_empty());
    }

    /// Asserts that `__supervise-native` is answered here, with status 2 and a usage line,
    /// for a command line with no `--spec` and for a spec file that does not exist, and that
    /// nothing is written to its output (the job's). Catches a supervisor left to the agent's
    /// own start, which would install signal handlers and start threads before the job.
    #[test]
    fn the_supervisor_is_answered_here_before_anything_else() {
        use crate::mac_supervisor::{VERB, status};

        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "hidden-supervise");
        let absent = dir.join("spec.json");
        let absent = absent.to_str().unwrap();
        for args in [
            &["js-agent", VERB][..],
            &["js-agent", VERB, "--spec", absent][..],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = dispatch(&words(args), &mut out, &mut err);
            assert_eq!(code, Some(ExitCode::from(status::USAGE)), "{args:?}");
            let err = String::from_utf8(err).unwrap();
            assert!(err.starts_with(&format!("js-agent: {VERB}: ")), "{err}");
            assert!(
                err.ends_with("usage: js-agent __supervise-native --spec <file>\n"),
                "{err}"
            );
            assert!(out.is_empty());
        }
    }

    /// Asserts that on Linux `image-pull` is answered here, before every other verb: a
    /// command line with no image is refused by the verb itself, which names itself, and
    /// nothing is pulled.
    #[cfg(target_os = "linux")]
    #[test]
    fn image_pull_is_answered_here_on_linux() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = words(&["js-agent", crate::image::pull::VERB]);
        let code = dispatch(&args, &mut out, &mut err);
        assert!(code.is_some_and(|code| code != ExitCode::SUCCESS));
        let err = String::from_utf8(err).unwrap();
        assert!(err.starts_with("js-agent image-pull: "), "{err}");
        assert!(out.is_empty());
    }
}
