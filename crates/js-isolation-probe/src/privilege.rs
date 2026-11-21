//! Privilege probes that change nothing: `sudo` and `su` without a password, membership of
//! `admin`, and a write of its own record's shell with the value it already has. (The
//! privilege probes that make something, a launchd job, a crontab, an at job and an
//! attribute on the record, are [`crate::marks::try_and_undo`].)

use crate::host::Who;
use crate::outcome::{Line, Outcome};
use crate::tools::Tools;

const SUDO: &str = "/usr/bin/sudo";
const SU: &str = "/usr/bin/su";
const TRUE: &str = "/usr/bin/true";
const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";
const DSCL: &str = "/usr/bin/dscl";

/// `priv:sudo`: `sudo -n true`, which never asks for a password.
#[must_use]
pub fn sudo(tools: &dyn Tools) -> Line {
    tools.run(SUDO, &["-n", TRUE], b"").line("priv:sudo")
}

/// `priv:su`: `su -l root -c true` with no input, so no password can be given.
#[must_use]
pub fn su(tools: &dyn Tools) -> Line {
    tools
        .run(SU, &["-l", "root", "-c", TRUE], b"")
        .line("priv:su")
}

/// `priv:admin-group`: whether the account is a member of `admin` (allowed means it is).
#[must_use]
pub fn admin(tools: &dyn Tools, who: &Who) -> Line {
    let name = "priv:admin-group";
    let ran = tools.run(
        DSEDITGROUP,
        &["-o", "checkmember", "-m", &who.user, "admin"],
        b"",
    );
    match ran.code {
        Some(0) => Line::new(name, Outcome::Allowed).detail("a member of admin"),
        // dseditgroup's answer for "not a member".
        Some(67) => Line::new(name, Outcome::Denied).detail("not a member of admin"),
        _ => {
            let mut line = ran.line(name);
            if line.outcome == Outcome::Denied {
                line.outcome = Outcome::Error;
            }
            line
        }
    }
}

/// `priv:record-same-value`: writes the account's `UserShell` on its own record with the
/// value it has, so nothing changes even when the write is allowed.
#[must_use]
pub fn record_same_value(tools: &dyn Tools, who: &Who) -> Line {
    let name = "priv:record-same-value";
    let record = format!("/Users/{}", who.user);
    let read = tools.run(DSCL, &[".", "-read", &record, "UserShell"], b"");
    let shell = read
        .out()
        .strip_prefix("UserShell:")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    match shell {
        Some(shell) if read.ok() => with_value(
            tools
                .run(DSCL, &[".", "-create", &record, "UserShell", &shell], b"")
                .line(name),
            &shell,
        ),
        _ => {
            Line::new(name, Outcome::Error).detail(format!("cannot read the shell: {}", read.why()))
        }
    }
}

/// The line with the value written put first in its detail.
fn with_value(mut line: Line, value: &str) -> Line {
    let said = format!("UserShell {value}");
    line.detail = if line.detail.is_empty() {
        said
    } else {
        format!("{said}; {}", line.detail)
    };
    line
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::host::test_who;
    use crate::tools::fake::Fake;

    #[test]
    fn sudo_and_su_are_denied_when_they_ask_for_a_password() {
        let tools = Fake::default()
            .exit(
                "/usr/bin/sudo -n /usr/bin/true",
                1,
                "",
                "sudo: a password is required",
            )
            .exit("/usr/bin/su -l root -c /usr/bin/true", 1, "", "su: Sorry");
        assert_eq!(
            sudo(&tools).to_string(),
            "priv:sudo: denied; exit 1: sudo: a password is required"
        );
        assert_eq!(su(&tools).to_string(), "priv:su: denied; exit 1: su: Sorry");
    }

    #[test]
    fn admin_membership_is_read_from_dseditgroups_exit() {
        let who = test_who(Path::new("/Users/pool-1"));
        let line = "/usr/sbin/dseditgroup -o checkmember -m pool-1 admin";
        let member = Fake::default().exit(line, 0, "yes pool-1 is a member of admin", "");
        assert_eq!(admin(&member, &who).outcome, Outcome::Allowed);
        let not = Fake::default().exit(line, 67, "no pool-1 is NOT a member of admin", "");
        assert_eq!(
            admin(&not, &who).to_string(),
            "priv:admin-group: denied; not a member of admin"
        );
        let odd = Fake::default().exit(line, 2, "", "Group not found.");
        assert_eq!(
            admin(&odd, &who).to_string(),
            "priv:admin-group: error; exit 2: Group not found."
        );
        let missing = Fake::default().on(
            line,
            crate::tools::Ran::unstarted(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        assert_eq!(admin(&missing, &who).outcome, Outcome::NotApplicable);
    }

    #[test]
    fn the_shell_is_written_with_the_value_it_has() {
        let who = test_who(Path::new("/Users/pool-1"));
        let tools = Fake::default()
            .exit(
                "/usr/bin/dscl . -read /Users/pool-1 UserShell",
                0,
                "UserShell: /bin/zsh\n",
                "",
            )
            .exit(
                "/usr/bin/dscl . -create /Users/pool-1 UserShell /bin/zsh",
                40,
                "",
                "<main> attribute status: eDSPermissionError",
            );
        assert_eq!(
            record_same_value(&tools, &who).to_string(),
            "priv:record-same-value: denied; UserShell /bin/zsh; exit 40: <main> attribute status: eDSPermissionError"
        );
        let allowed = Fake::default()
            .exit(
                "/usr/bin/dscl . -read /Users/pool-1 UserShell",
                0,
                "UserShell: /bin/zsh",
                "",
            )
            .exit(
                "/usr/bin/dscl . -create /Users/pool-1 UserShell /bin/zsh",
                0,
                "",
                "",
            );
        assert_eq!(
            record_same_value(&allowed, &who).to_string(),
            "priv:record-same-value: allowed; UserShell /bin/zsh"
        );
    }

    #[test]
    fn a_shell_that_cannot_be_read_is_an_error_and_nothing_is_written() {
        let who = test_who(Path::new("/Users/pool-1"));
        for (code, out) in [
            (1, ""),
            (0, "Shell: /bin/zsh"),
            (0, "UserShell:  "),
            (1, "UserShell: /bin/zsh"),
        ] {
            let tools = Fake::default().exit(
                "/usr/bin/dscl . -read /Users/pool-1 UserShell",
                code,
                out,
                "",
            );
            let line = record_same_value(&tools, &who);
            assert_eq!(line.outcome, Outcome::Error, "{line}");
            assert!(line.detail.starts_with("cannot read the shell: "), "{line}");
            assert_eq!(tools.lines().len(), 1, "only the read was asked");
        }
    }
}
