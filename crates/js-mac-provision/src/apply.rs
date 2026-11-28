//! [`apply()`]: loads the anchor the agent's flags file names, unless it is in force already.
//!
//! The pf daemon runs it at load and every 30 seconds (`js-agent __pf-anchor --flags-file`), so
//! that a run reloads nothing under the agent's own checks. The anchor, the state directory and
//! the `pfctl` are the file's (the agent's defaults when it names none): they are the ones the
//! agent checks with.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::agent_flags::{self, AgentPf, FileFacts, FlagsError};
use crate::pfctl::Pfctl;
use crate::policy::{AnchorName, UidRange};
use crate::provision::{ProvisionError, Provisioner};

/// What [`apply()`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// There is no flags file: nothing was run.
    NoFile {
        /// The flags file that is not there.
        path: PathBuf,
    },
    /// The file sets no `--native-network=on`: the agent checks no anchor, nothing was run.
    Off {
        /// The flags file.
        path: PathBuf,
        /// The ceiling the file gives.
        network: String,
    },
    /// The anchor was in force already: nothing was loaded.
    InForce {
        /// The anchor.
        anchor: AnchorName,
        /// The uids it limits.
        uids: UidRange,
    },
    /// The anchor was loaded.
    Installed {
        /// The anchor.
        anchor: AnchorName,
        /// The uids it limits.
        uids: UidRange,
    },
}

impl fmt::Display for Applied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Applied::NoFile { path } => {
                write!(f, "no flags file {}: nothing to install", path.display())
            }
            Applied::Off { path, network } => write!(
                f,
                "{}: {}={network}: the agent checks no anchor, nothing to install",
                path.display(),
                agent_flags::NETWORK
            ),
            Applied::InForce { anchor, uids } => {
                write!(f, "in force: {anchor} holds the rules for uids {uids}")
            }
            Applied::Installed { anchor, uids } => {
                write!(f, "installed {anchor} for uids {uids}")
            }
        }
    }
}

/// Why [`apply()`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The flags file was refused.
    #[error(transparent)]
    Flags(#[from] FlagsError),
    /// The anchor could not be checked or loaded.
    #[error(transparent)]
    Provision(#[from] ProvisionError),
}

/// Loads the anchor the flags file at `file` names, unless `check` says it is in force already.
/// `euid` is the user the file may belong to besides root, and `facts` looks at the file
/// ([`agent_flags::read_system`] on a machine). `pfctl_of` makes the runner of the `pfctl` the
/// file names.
///
/// # Errors
/// [`ApplyError`]: the file is refused, or the anchor could not be loaded.
pub fn apply<'a>(
    file: &Path,
    euid: u32,
    facts: &dyn Fn(&Path) -> io::Result<FileFacts>,
    pfctl_of: &dyn Fn(&Path) -> Box<dyn Pfctl + 'a>,
) -> Result<Applied, ApplyError> {
    let policy = match agent_flags::read(file, euid, facts)? {
        AgentPf::NoFile => {
            return Ok(Applied::NoFile {
                path: file.to_path_buf(),
            });
        }
        AgentPf::Off { network } => {
            return Ok(Applied::Off {
                path: file.to_path_buf(),
                network,
            });
        }
        AgentPf::On(policy) => policy,
    };
    let pf = pfctl_of(&policy.pfctl);
    let provisioner = Provisioner::new(&*pf, &policy.anchor, &policy.state_dir);
    let (uids, anchor) = (policy.uids, policy.anchor.clone());
    if provisioner.check(uids, &policy.endpoints).is_ok() {
        Ok(Applied::InForce { anchor, uids })
    } else {
        provisioner.install(uids, &policy.endpoints)?;
        Ok(Applied::Installed { anchor, uids })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use js_testkit::dir::TestDir;

    use super::*;
    use crate::agent_flags::read_system;
    use crate::pfctl::{Output, SystemPfctl};
    use crate::testing::Fake;

    /// The fake, lent to the runner `apply` asks for.
    struct Lent<'a>(&'a Fake);

    impl Pfctl for Lent<'_> {
        fn run(&self, args: &[&str]) -> io::Result<Output> {
            self.0.run(args)
        }
    }

    /// The runner `apply` asks for: `pf` for every `pfctl`, but `/nonexistent/pfctl`, which is
    /// the real runner of a program that is not there. Each path asked for is noted in `asked`.
    fn runner_of<'a>(
        pf: &'a Fake,
        asked: &'a RefCell<Vec<PathBuf>>,
    ) -> impl Fn(&Path) -> Box<dyn Pfctl + 'a> + 'a {
        move |path| {
            asked.borrow_mut().push(path.to_path_buf());
            if path == Path::new("/nonexistent/pfctl") {
                Box::new(SystemPfctl::new(path))
            } else {
                Box::new(Lent(pf))
            }
        }
    }

    /// Runs `apply` on `file` as its owner. Returns the result and the `pfctl` paths the
    /// runner was asked for.
    fn run(dir: &Path, file: &Path, pf: &Fake) -> (Result<Applied, ApplyError>, Vec<PathBuf>) {
        let euid = std::fs::metadata(dir).unwrap().uid();
        let asked = RefCell::new(Vec::new());
        let result = apply(file, euid, &read_system, &runner_of(pf, &asked));
        (result, asked.into_inner())
    }

    /// The agent's flags for the pool 600-699 and one endpoint, with the state directory `dir`.
    fn agent_flags(dir: &Path) -> String {
        format!(
            "--server=https://leases.example:7300\n--native-network=on\n\
             --native-pf-uid-start=600\n--native-pf-uid-count=100\n\
             --native-pf-endpoint=192.0.2.10:443\n--native-pf-state-dir={}\n",
            dir.display()
        )
    }

    /// Writes a flags file as the agent's own: not writable by group or others, whatever the
    /// umask.
    fn write_flags(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_flags_file_loads_the_agents_anchor_and_then_reloads_nothing() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-load");
        let file = dir.join("flags");
        write_flags(&file, &agent_flags(dir.path()));
        let pf = Fake::new(dir.path());
        let (r, _) = run(dir.path(), &file, &pf);
        let applied = r.unwrap();
        assert!(matches!(applied, Applied::Installed { .. }), "{applied:?}");
        assert_eq!(
            applied.to_string(),
            "installed com.apple/com.komira.js.pool for uids 600-699"
        );
        let load = format!(
            "-a com.apple/com.komira.js.pool -f {}",
            dir.join("pool.pf").display()
        );
        assert_eq!(pf.calls().iter().filter(|c| **c == load).count(), 1);
        let rules = std::fs::read_to_string(dir.join("pool.pf")).unwrap();
        assert!(
            rules.contains("to 192.0.2.10 port 443 user { 600, 601, "),
            "{rules}"
        );
        let (r, _) = run(dir.path(), &file, &pf);
        let applied = r.unwrap();
        assert!(matches!(applied, Applied::InForce { .. }), "{applied:?}");
        assert_eq!(
            applied.to_string(),
            "in force: com.apple/com.komira.js.pool holds the rules for uids 600-699"
        );
        assert_eq!(pf.calls().iter().filter(|c| **c == load).count(), 1);
    }

    #[test]
    fn a_flags_file_whose_endpoints_changed_loads_the_new_rules() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-change");
        let file = dir.join("flags");
        write_flags(&file, &agent_flags(dir.path()));
        let pf = Fake::new(dir.path());
        assert!(run(dir.path(), &file, &pf).0.is_ok());
        let changed = agent_flags(dir.path()).replace("192.0.2.10:443", "198.51.100.7:8443");
        write_flags(&file, &changed);
        let (r, _) = run(dir.path(), &file, &pf);
        assert_eq!(
            r.unwrap().to_string(),
            "installed com.apple/com.komira.js.pool for uids 600-699"
        );
        let rules = std::fs::read_to_string(dir.join("pool.pf")).unwrap();
        assert!(rules.contains("to 198.51.100.7 port 8443 user"), "{rules}");
        assert!(!rules.contains("192.0.2.10"), "{rules}");
    }

    #[test]
    fn a_flags_file_that_is_absent_or_not_on_runs_no_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-nothing");
        let pf = Fake::new(dir.path());
        let absent = dir.join("absent");
        let (r, asked) = run(dir.path(), &absent, &pf);
        let applied = r.unwrap();
        assert_eq!(
            applied,
            Applied::NoFile {
                path: absent.clone()
            }
        );
        assert_eq!(
            applied.to_string(),
            format!("no flags file {}: nothing to install", absent.display())
        );
        assert!(asked.is_empty());
        let off = dir.join("off");
        write_flags(
            &off,
            "--server=https://leases.example:7300\n--native-network=open\n",
        );
        let (r, asked) = run(dir.path(), &off, &pf);
        let applied = r.unwrap();
        assert_eq!(
            applied,
            Applied::Off {
                path: off.clone(),
                network: "open".to_owned()
            }
        );
        assert_eq!(
            applied.to_string(),
            format!(
                "{}: --native-network=open: the agent checks no anchor, nothing to install",
                off.display()
            )
        );
        assert!(asked.is_empty());
        assert!(pf.calls().is_empty());
        assert!(!dir.join("pool.pf").exists());
    }

    /// The anchor, the state directory and the `pfctl` are the ones the flags file names, which
    /// the agent checks with: a daemon that runs with no flag of its own could never load a
    /// non-default anchor the agent accepts.
    #[test]
    fn a_flags_file_names_the_anchor_and_the_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-named");
        let pf = Fake::new(dir.path());
        let file = dir.join("flags");
        let other = format!(
            "{}--native-pf-anchor=com.apple/other\n",
            agent_flags(dir.path())
        );
        write_flags(&file, &other);
        let (r, asked) = run(dir.path(), &file, &pf);
        assert_eq!(
            r.unwrap().to_string(),
            "installed com.apple/other for uids 600-699"
        );
        assert_eq!(asked, [PathBuf::from("/sbin/pfctl")]);
        let load = format!("-a com.apple/other -f {}", dir.join("pool.pf").display());
        assert!(pf.calls().contains(&load), "{:?}", pf.calls());
        let named = format!("{other}--native-pfctl=/nonexistent/pfctl\n");
        write_flags(&file, &named);
        let calls = pf.calls().len();
        let (r, asked) = run(dir.path(), &file, &pf);
        assert_eq!(asked, [PathBuf::from("/nonexistent/pfctl")]);
        let e = r.unwrap_err();
        assert!(matches!(e, ApplyError::Provision(_)), "{e:?}");
        assert!(e.to_string().contains("could not run pfctl"), "{e}");
        assert_eq!(pf.calls().len(), calls);
    }

    #[test]
    fn a_refused_flags_file_runs_no_pfctl() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-refused");
        let pf = Fake::new(dir.path());
        let file = dir.join("flags");
        write_flags(&file, "--native-network=on\n--native-network=off\n");
        let (r, asked) = run(dir.path(), &file, &pf);
        let e = r.unwrap_err();
        assert!(matches!(e, ApplyError::Flags(_)), "{e:?}");
        assert!(
            e.to_string()
                .ends_with("--native-network: given more than once"),
            "{e}"
        );
        assert!(asked.is_empty());
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn a_failed_step_is_a_provision_error() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-fail");
        let file = dir.join("flags");
        write_flags(&file, &agent_flags(dir.path()));
        let mut pf = Fake::new(dir.path());
        // Call 0 is the check's; call 1 is the install's first.
        pf.fail = Some((1, false));
        let (r, _) = run(dir.path(), &file, &pf);
        let e = r.unwrap_err();
        assert!(matches!(e, ApplyError::Provision(_)), "{e:?}");
        assert!(e.to_string().contains("exited with 1"), "{e}");
    }

    #[test]
    fn a_flags_file_with_no_uids_is_refused_with_the_flag_named() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-apply-no-uids");
        let pf = Fake::new(dir.path());
        let file = dir.join("flags");
        write_flags(&file, "--native-network=on\n");
        let (r, _) = run(dir.path(), &file, &pf);
        let e = r.unwrap_err();
        assert!(
            e.to_string()
                .contains("--native-pf-uid-start: required with --native-network on"),
            "{e}"
        );
    }
}
