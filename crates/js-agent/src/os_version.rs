//! The major version of the host's operating system, reported as the capability
//! `os_version` ([`js_caps::OS_VERSION`]) so a job that asks for a least version (`26`: macOS
//! 26 or newer) is placed only on a host that has it.
//!
//! Only a Mac reports it. The version is read with `sysctl -n kern.osproductversion`, run by
//! its absolute path through the agent's [`CommandRunner`] like the other probes, and
//! printed as `26`, `26.1` or `15.7.2`; the major is the part before the first dot. A
//! probe that fails, or prints text with no major version, reports no version and a warning:
//! the host then serves no job that asks for one, and still serves the others.

use crate::facts::{CommandRunner, Platform};

/// The program that prints the product version of macOS.
const SYSCTL: &str = "/usr/sbin/sysctl";

/// The major version of the host's operating system, as a decimal string, and the warning to
/// log when it could not be read. A host that is not a Mac reports neither.
pub(crate) fn report(
    platform: &Platform,
    runner: &dyn CommandRunner,
) -> (Option<String>, Option<String>) {
    match platform.is_macos() {
        false => (None, None),
        true => match major(runner) {
            Ok(version) => (Some(version), None),
            Err(why) => (
                None,
                Some(format!(
                    "OS version probe failed ({why}); reporting no os_version"
                )),
            ),
        },
    }
}

/// The major version `sysctl -n kern.osproductversion` prints: `26.1` is `26`, `26` is `26`.
///
/// # Errors
///
/// `sysctl` cannot be run, or its output does not begin with a whole major version of at
/// least 1.
fn major(runner: &dyn CommandRunner) -> Result<String, String> {
    let text = runner.run(SYSCTL, &["-n", "kern.osproductversion"])?;
    let text = text.trim();
    text.split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .filter(|major| *major >= 1)
        .map(|major| major.to_string())
        .ok_or_else(|| format!("sysctl printed {text:?}, which is not a version"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A runner that answers every command with one fixed result and records what it was
    /// asked to run.
    struct Fixed {
        answer: Result<String, String>,
        asked: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Fixed {
        fn printing(text: &str) -> Self {
            Self {
                answer: Ok(text.to_owned()),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for Fixed {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
            self.asked.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.answer.clone()
        }

        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
    }

    fn platform(os: &str) -> Platform {
        Platform {
            os: os.to_owned(),
            arch: "arm64".to_owned(),
        }
    }

    /// Asserts that a Mac reports the major of the version `sysctl` prints, whether it prints
    /// a major alone, a minor, a patch level or a trailing newline, and that the program run
    /// is `/usr/sbin/sysctl -n kern.osproductversion`. Catches a probe that reports the
    /// whole version text, which no job's `os_version` equals, or one that runs a program
    /// found on the `PATH`.
    #[test]
    fn a_mac_reports_the_major_version_sysctl_prints() {
        for (printed, major) in [
            ("26\n", "26"),
            ("26.1\n", "26"),
            ("15.7.2\n", "15"),
            ("  14.0 ", "14"),
        ] {
            let runner = Fixed::printing(printed);
            assert_eq!(
                report(&platform("macos"), &runner),
                (Some(major.to_owned()), None),
                "{printed:?}"
            );
            assert_eq!(
                runner.asked.into_inner(),
                vec![(
                    "/usr/sbin/sysctl".to_owned(),
                    vec!["-n".to_owned(), "kern.osproductversion".to_owned()]
                )]
            );
        }
    }

    /// Asserts that output that is no version (empty, text, a zero or negative major, a
    /// major that does not fit) and a command that fails each report no version and one
    /// warning naming the cause, and that a Linux host reports nothing and runs nothing.
    /// Catches a host that reports a garbage version for a job to be placed by, or that
    /// fails its hello for want of one.
    #[test]
    fn an_unreadable_version_is_no_version_with_a_warning_and_linux_reports_none() {
        for printed in ["", "\n", "beta", "0", "0.9", "-1", ".5", "99999999999"] {
            let (version, warning) = report(&platform("macos"), &Fixed::printing(printed));
            assert_eq!(version, None, "{printed:?}");
            assert_eq!(
                warning,
                Some(format!(
                    "OS version probe failed (sysctl printed {:?}, which is not a version); reporting no os_version",
                    printed.trim()
                )),
                "{printed:?}"
            );
        }
        let failing = Fixed {
            answer: Err("/usr/sbin/sysctl: not found".to_owned()),
            asked: RefCell::new(Vec::new()),
        };
        assert_eq!(
            report(&platform("macos"), &failing),
            (
                None,
                Some(
                    "OS version probe failed (/usr/sbin/sysctl: not found); reporting no os_version"
                        .to_owned()
                )
            )
        );
        let linux = Fixed::printing("26.1");
        assert_eq!(report(&platform("linux"), &linux), (None, None));
        assert!(linux.asked.into_inner().is_empty());
    }
}
