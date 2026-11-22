//! The hidden verbs [`crate::main_with`] answers first, before the signal handlers are
//! installed and before any thread starts: each is a short-lived program of its own that runs
//! no agent.
//!
//! - `image-pull` (Linux): pulls an image into the image store ([`crate::image::pull`]).
//! - `__mac-job` and `__mac-job-start`: one native Mac job run by this program alone, and its
//!   start in a process of its own ([`crate::mac_job`]); a prototype of the one Mac program.
//! - `__account-cycle`: a development verb that runs the life cycle of one account of the
//!   Mac account pool once, to prove it on a Mac ([`crate::mac_accounts::cycle`]). It runs no
//!   job, and is removed once the native runtime proves an account's life cycle itself.

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
    #[cfg(target_os = "linux")]
    if verb == crate::image::pull::VERB {
        let host = crate::image::pull::Host::system();
        return Some(crate::image::pull::run(args, out, err, &host));
    }
    if let Some(code) = crate::mac_job::dispatch(args, out, err) {
        return Some(code);
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
            assert!(out.is_empty() && err.is_empty(), "{args:?}");
        }
    }
}
