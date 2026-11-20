//! The pf policy that the macOS agent's flags file gives: what `install --flags-file` loads.
//!
//! A Mac's agent reads its host's own flags from a file (`js-agent --flags-file
//! /etc/js-agent/flags`, one argument per line, such as `--native-pf-uid-start=600`). Its
//! `--native-network` and `--native-pf-*` flags name the anchor the agent checks before it runs a
//! job with the network on. [`read`] takes the same flags from the same file, so that the anchor
//! the LaunchDaemon loads is the one the agent checks, and a configuration package that changes
//! the file changes both:
//!
//! - the file is trusted as the agent trusts it: a regular file (a link is followed), owned by
//!   root or by the user running this program, not writable by group or others, at most
//!   [`MAX_BYTES`] of UTF-8;
//! - a line is one argument with its surrounding whitespace trimmed, and a flag and its value are
//!   one line, `--flag=value`. Only the flags named here are read; every other line is the
//!   agent's (comments and empty lines included) and is skipped;
//! - `--native-network` (`off` when absent; `off`, `loopback`, `on` or `open`) decides: with any
//!   value but `on` there is no anchor to load ([`AgentPf::Off`]), and a pf flag beside it is
//!   refused, as the agent refuses it;
//! - with `on`, `--native-pf-uid-start` and `--native-pf-uid-count` are required, each
//!   `--native-pf-endpoint` is an endpoint, and `--native-pf-anchor`, `--native-pf-state-dir`
//!   and `--native-pfctl` take the agent's defaults when absent; the two paths are absolute;
//! - a flag read here other than `--native-pf-endpoint` may be given once.
//!
//! A file that is not there is [`AgentPf::NoFile`]: a host whose configuration package has not
//! arrived yet.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cli::{DEFAULT_ANCHOR, DEFAULT_STATE_DIR};
use crate::pfctl::SYSTEM_PFCTL;
use crate::policy::{AnchorName, Endpoint, PolicyError, UidRange};

/// The most bytes a flags file may hold, as the agent allows.
pub const MAX_BYTES: u64 = 64 * 1024;

/// The agent's network ceiling.
pub const NETWORK: &str = "--native-network";
/// The `pfctl` the agent runs.
pub const PFCTL: &str = "--native-pfctl";
/// The anchor the agent checks.
pub const ANCHOR: &str = "--native-pf-anchor";
/// The state directory of the anchor the agent checks.
pub const STATE_DIR: &str = "--native-pf-state-dir";
/// The first uid of the pool.
pub const UID_START: &str = "--native-pf-uid-start";
/// How many uids the pool holds.
pub const UID_COUNT: &str = "--native-pf-uid-count";
/// An endpoint the pool may reach; the one flag that may be repeated.
pub const ENDPOINT: &str = "--native-pf-endpoint";

/// Every flag read here: the network ceiling first, then the pf flags.
const FLAGS: [&str; 7] = [
    NETWORK, PFCTL, ANCHOR, STATE_DIR, UID_START, UID_COUNT, ENDPOINT,
];

/// What is known of a flags file: the facts [`read`] judges it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether it is a regular file after following links.
    pub regular: bool,
    /// Its permission bits.
    pub mode: u32,
    /// Its owner.
    pub uid: u32,
    /// Its content, at most [`MAX_BYTES`] and one byte more when the file is larger; empty when
    /// it is not a regular file.
    pub bytes: Vec<u8>,
}

/// The anchor the agent's flags name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// `--native-pfctl`, or `/sbin/pfctl`.
    pub pfctl: PathBuf,
    /// `--native-pf-anchor`, or the default anchor.
    pub anchor: AnchorName,
    /// `--native-pf-state-dir`, or the default state directory.
    pub state_dir: PathBuf,
    /// `--native-pf-uid-start` and `--native-pf-uid-count`.
    pub uids: UidRange,
    /// Each `--native-pf-endpoint`, in the file's order.
    pub endpoints: Vec<Endpoint>,
}

/// What the flags file says of the anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentPf {
    /// There is no flags file.
    NoFile,
    /// The network ceiling is not `on`: the agent checks no anchor.
    Off {
        /// The ceiling the file gives (`off` when it gives none).
        network: String,
    },
    /// The network ceiling is `on`, and the agent checks this anchor.
    On(Policy),
}

/// Why a flag of the file was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlagFault {
    /// The flag was given more than once.
    #[error("given more than once")]
    Repeated,
    /// The flag's line has no `=value`.
    #[error("has no value: the line must be the flag, = and the value")]
    NoValue,
    /// `--native-network` is not one of the agent's ceilings.
    #[error("{0:?} is not one of off, loopback, on, open")]
    Network(String),
    /// A pf flag beside a network ceiling that is not `on`.
    #[error("needs --native-network on")]
    NeedsOn,
    /// A pf flag that `on` requires is absent.
    #[error("required with --native-network on")]
    Missing,
    /// A uid flag that is not a whole number.
    #[error("{0:?} is not a whole number")]
    NotANumber(String),
    /// A path that is not absolute.
    #[error("{0} is not an absolute path")]
    Relative(String),
    /// The provisioner refuses the value.
    #[error(transparent)]
    Policy(PolicyError),
}

/// Why a flags file was refused.
#[derive(Debug, thiserror::Error)]
pub enum FlagsError {
    /// The file is there and cannot be read.
    #[error("{}: cannot be read: {source}", path.display())]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
    /// The file, after following links, is not a regular file.
    #[error("{}: not a regular file", path.display())]
    NotRegular {
        /// The file.
        path: PathBuf,
    },
    /// Group or others can write the file.
    #[error("{}: writable by group or others (mode {mode:04o})", path.display())]
    Writable {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file belongs to neither root nor the user running this program.
    #[error("{}: owned by user {uid}, not by root or the user running this program", path.display())]
    Owner {
        /// The file.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
    /// The file is larger than [`MAX_BYTES`].
    #[error("{}: larger than {MAX_BYTES} bytes", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file is not UTF-8.
    #[error("{}: not UTF-8 text", path.display())]
    NotUtf8 {
        /// The file.
        path: PathBuf,
    },
    /// A flag of the file was refused.
    #[error("{}: {flag}: {fault}", path.display())]
    Flag {
        /// The file.
        path: PathBuf,
        /// The flag.
        flag: &'static str,
        /// Why.
        fault: FlagFault,
    },
}

/// Reads the facts of the file at `path` from the system. A link is followed; a file that is not
/// regular is not opened, so a named pipe is not waited on.
///
/// # Errors
/// The file cannot be looked at or read.
pub fn read_system(path: &Path) -> io::Result<FileFacts> {
    let meta = fs::metadata(path)?;
    let mut bytes = Vec::new();
    if meta.is_file() {
        fs::File::open(path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
    }
    Ok(FileFacts {
        regular: meta.is_file(),
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        bytes,
    })
}

/// What the flags file at `path` says of the anchor. `euid` is the user the file may belong to
/// besides root; `facts` looks at the file ([`read_system`] on a machine).
///
/// # Errors
/// [`FlagsError`]: the file is there and cannot be read, is not a regular file, is writable by
/// group or others, belongs to another user, is too large or not UTF-8, or a flag read here is
/// refused (see the module's rules).
pub fn read(
    path: &Path,
    euid: u32,
    facts: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<AgentPf, FlagsError> {
    let owned = || path.to_path_buf();
    let facts = match facts(path) {
        Ok(facts) => facts,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(AgentPf::NoFile),
        Err(source) => {
            return Err(FlagsError::Unreadable {
                path: owned(),
                source,
            });
        }
    };
    if !facts.regular {
        return Err(FlagsError::NotRegular { path: owned() });
    }
    if facts.mode & 0o022 != 0 {
        return Err(FlagsError::Writable {
            path: owned(),
            mode: facts.mode,
        });
    }
    if facts.uid != euid && facts.uid != 0 {
        return Err(FlagsError::Owner {
            path: owned(),
            uid: facts.uid,
        });
    }
    if facts.bytes.len() as u64 > MAX_BYTES {
        return Err(FlagsError::TooLarge { path: owned() });
    }
    let Ok(text) = std::str::from_utf8(&facts.bytes) else {
        return Err(FlagsError::NotUtf8 { path: owned() });
    };
    pf_of(text).map_err(|(flag, fault)| FlagsError::Flag {
        path: owned(),
        flag,
        fault,
    })
}

/// A flag and why it was refused.
type Refused = (&'static str, FlagFault);

/// The anchor of a flags file's text.
fn pf_of(text: &str) -> Result<AgentPf, Refused> {
    let mut given: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        let (name, value) = match line.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (line, None),
        };
        let Some(flag) = FLAGS.into_iter().find(|flag| *flag == name) else {
            continue;
        };
        let value = value.ok_or((flag, FlagFault::NoValue))?;
        given.entry(flag).or_default().push(value);
    }
    let one = |flag: &'static str| match given.get(flag).map(Vec::as_slice) {
        Some([value]) => Ok(Some(*value)),
        Some(_) => Err((flag, FlagFault::Repeated)),
        None => Ok(None),
    };
    let network = one(NETWORK)?.unwrap_or("off");
    if network != "on" {
        if !matches!(network, "off" | "loopback" | "open") {
            return Err((NETWORK, FlagFault::Network(network.to_owned())));
        }
        if let Some(flag) = FLAGS[1..]
            .iter()
            .copied()
            .find(|flag| given.contains_key(flag))
        {
            return Err((flag, FlagFault::NeedsOn));
        }
        return Ok(AgentPf::Off {
            network: network.to_owned(),
        });
    }
    let number = |flag: &'static str| -> Result<u32, Refused> {
        let text = one(flag)?.ok_or((flag, FlagFault::Missing))?;
        text.parse()
            .map_err(|_| (flag, FlagFault::NotANumber(text.to_owned())))
    };
    let uids = UidRange::new(number(UID_START)?, number(UID_COUNT)?)
        .map_err(|why| (UID_START, FlagFault::Policy(why)))?;
    let anchor = AnchorName::new(one(ANCHOR)?.unwrap_or(DEFAULT_ANCHOR))
        .map_err(|why| (ANCHOR, FlagFault::Policy(why)))?;
    let endpoints = given
        .get(ENDPOINT)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|text| {
            text.parse()
                .map_err(|why: PolicyError| (ENDPOINT, FlagFault::Policy(why)))
        })
        .collect::<Result<Vec<Endpoint>, Refused>>()?;
    let absolute = |flag: &'static str, default: &str| -> Result<PathBuf, Refused> {
        let path = PathBuf::from(one(flag)?.unwrap_or(default));
        if path.is_absolute() {
            Ok(path)
        } else {
            Err((flag, FlagFault::Relative(path.display().to_string())))
        }
    };
    Ok(AgentPf::On(Policy {
        pfctl: absolute(PFCTL, SYSTEM_PFCTL)?,
        anchor,
        state_dir: absolute(STATE_DIR, DEFAULT_STATE_DIR)?,
        uids,
        endpoints,
    }))
}

#[cfg(test)]
mod tests {
    use js_testkit::dir::TestDir;

    use super::*;

    const OWNER: u32 = 501;

    /// A file of `text` as the agent's own: regular, 0644, owned by [`OWNER`].
    fn file(text: &str) -> FileFacts {
        FileFacts {
            regular: true,
            mode: 0o644,
            uid: OWNER,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn read_facts(facts: FileFacts) -> Result<AgentPf, FlagsError> {
        read(Path::new("/etc/js-agent/flags"), OWNER, &move |_| {
            Ok(facts.clone())
        })
    }

    fn read_text(text: &str) -> Result<AgentPf, FlagsError> {
        read_facts(file(text))
    }

    /// The flag and the fault of a refused flag.
    fn refused(text: &str) -> (&'static str, FlagFault) {
        match read_text(text) {
            Err(FlagsError::Flag { flag, fault, .. }) => (flag, fault),
            other => panic!("{text:?}: {other:?}"),
        }
    }

    const MAC: &str = "# the Mac build host group\n\
        --server=https://leases.example:7300\n\
        --pool=mac\n\
        --host-id-from-facts=/var/lib/hostkeeper/facts.json\n\
        \n\
        --native-network=on\n\
        --native-pf-uid-start=600\n\
        \t--native-pf-uid-count=100  \n\
        --native-pf-endpoint=198.51.100.7:8443\n\
        --native-pf-endpoint=192.0.2.10:443\n";

    #[test]
    fn the_mac_flags_give_the_pool_its_endpoints_and_the_agents_defaults() {
        let AgentPf::On(policy) = read_text(MAC).unwrap() else {
            panic!("not on");
        };
        assert_eq!(
            policy,
            Policy {
                pfctl: PathBuf::from("/sbin/pfctl"),
                anchor: AnchorName::new("com.apple/com.komira.js.pool").unwrap(),
                state_dir: PathBuf::from("/var/db/com.komira.js-pf"),
                uids: UidRange::new(600, 100).unwrap(),
                endpoints: vec![
                    "198.51.100.7:8443".parse().unwrap(),
                    "192.0.2.10:443".parse().unwrap(),
                ],
            }
        );
    }

    #[test]
    fn the_anchor_the_state_directory_and_pfctl_are_read_when_given() {
        let text = "--native-network=on\n--native-pf-uid-start=700\n--native-pf-uid-count=1\n\
            --native-pf-anchor=com.apple/other\n--native-pf-state-dir=/var/db/other\n\
            --native-pfctl=/opt/pfctl\n";
        let AgentPf::On(policy) = read_text(text).unwrap() else {
            panic!("not on");
        };
        assert_eq!(policy.anchor.as_str(), "com.apple/other");
        assert_eq!(policy.state_dir, PathBuf::from("/var/db/other"));
        assert_eq!(policy.pfctl, PathBuf::from("/opt/pfctl"));
        assert_eq!(policy.uids, UidRange::new(700, 1).unwrap());
        assert!(policy.endpoints.is_empty());
    }

    #[test]
    fn a_ceiling_other_than_on_loads_nothing() {
        assert_eq!(
            read_text("--server=https://x:1\n").unwrap(),
            AgentPf::Off {
                network: "off".to_owned()
            }
        );
        for network in ["off", "loopback", "open"] {
            assert_eq!(
                read_text(&format!("--native-network={network}\n")).unwrap(),
                AgentPf::Off {
                    network: network.to_owned()
                },
                "{network}"
            );
        }
    }

    #[test]
    fn a_pf_flag_beside_a_ceiling_other_than_on_is_refused_by_its_name() {
        assert_eq!(
            refused("--native-network=open\n--native-pf-uid-start=600\n"),
            (UID_START, FlagFault::NeedsOn)
        );
        assert_eq!(
            refused("--native-pf-endpoint=192.0.2.10:443\n"),
            (ENDPOINT, FlagFault::NeedsOn)
        );
    }

    #[test]
    fn a_ceiling_the_agent_does_not_have_is_refused() {
        assert_eq!(
            refused("--native-network=yes\n"),
            (NETWORK, FlagFault::Network("yes".to_owned()))
        );
    }

    #[test]
    fn a_flag_read_here_without_its_value_or_given_twice_is_refused() {
        assert_eq!(refused("--native-network\n"), (NETWORK, FlagFault::NoValue));
        assert_eq!(
            refused("--native-network=on\n--native-network=on\n"),
            (NETWORK, FlagFault::Repeated)
        );
        assert_eq!(
            refused("--native-network=on\n--native-pf-uid-start=600\n--native-pf-uid-start=600\n"),
            (UID_START, FlagFault::Repeated)
        );
    }

    #[test]
    fn on_needs_the_pools_uids_as_whole_numbers_the_provisioner_accepts() {
        assert_eq!(
            refused("--native-network=on\n--native-pf-uid-count=100\n"),
            (UID_START, FlagFault::Missing)
        );
        assert_eq!(
            refused("--native-network=on\n--native-pf-uid-start=600\n"),
            (UID_COUNT, FlagFault::Missing)
        );
        assert_eq!(
            refused("--native-network=on\n--native-pf-uid-start=6x\n--native-pf-uid-count=1\n"),
            (UID_START, FlagFault::NotANumber("6x".to_owned()))
        );
        let (flag, fault) =
            refused("--native-network=on\n--native-pf-uid-start=400\n--native-pf-uid-count=1\n");
        assert_eq!(flag, UID_START);
        assert!(matches!(fault, FlagFault::Policy(_)), "{fault:?}");
        assert!(fault.to_string().contains("below 500"), "{fault}");
    }

    const ON: &str = "--native-network=on\n--native-pf-uid-start=600\n--native-pf-uid-count=1\n";

    #[test]
    fn an_anchor_an_endpoint_or_a_path_the_agent_refuses_is_refused() {
        let (flag, fault) = refused(&format!("{ON}--native-pf-anchor=a/*\n"));
        assert_eq!(flag, ANCHOR);
        assert!(matches!(fault, FlagFault::Policy(_)), "{fault:?}");
        let (flag, fault) = refused(&format!("{ON}--native-pf-endpoint=169.254.169.254:80\n"));
        assert_eq!(flag, ENDPOINT);
        assert!(fault.to_string().contains("link-local"), "{fault}");
        assert_eq!(
            refused(&format!("{ON}--native-pfctl=pfctl\n")),
            (PFCTL, FlagFault::Relative("pfctl".to_owned()))
        );
        assert_eq!(
            refused(&format!("{ON}--native-pf-state-dir=state\n")),
            (STATE_DIR, FlagFault::Relative("state".to_owned()))
        );
    }

    #[test]
    fn a_refusal_names_the_file_the_flag_and_why() {
        let e = read_text("--native-network=on\n--native-network=off\n").unwrap_err();
        assert_eq!(
            e.to_string(),
            "/etc/js-agent/flags: --native-network: given more than once"
        );
    }

    #[test]
    fn a_file_that_is_not_there_is_no_file() {
        let missing = read(Path::new("/x"), OWNER, &|_| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(missing.unwrap(), AgentPf::NoFile);
    }

    #[test]
    fn a_file_the_agent_would_not_trust_is_refused() {
        let unreadable = read(Path::new("/x"), OWNER, &|_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(matches!(unreadable, FlagsError::Unreadable { .. }));
        assert!(unreadable.to_string().starts_with("/x: cannot be read: "));
        let mut facts = file(ON);
        facts.regular = false;
        assert_eq!(
            read_facts(facts).unwrap_err().to_string(),
            "/etc/js-agent/flags: not a regular file"
        );
        let mut facts = file(ON);
        facts.mode = 0o664;
        assert_eq!(
            read_facts(facts).unwrap_err().to_string(),
            "/etc/js-agent/flags: writable by group or others (mode 0664)"
        );
        let mut facts = file(ON);
        facts.uid = OWNER + 1;
        assert_eq!(
            read_facts(facts).unwrap_err().to_string(),
            "/etc/js-agent/flags: owned by user 502, not by root or the user running this program"
        );
        let facts = file(&format!("{ON}#{}\n", "x".repeat(64 * 1024)));
        assert_eq!(
            read_facts(facts).unwrap_err().to_string(),
            "/etc/js-agent/flags: larger than 65536 bytes"
        );
        let mut facts = file(ON);
        facts.bytes.push(0xff);
        assert_eq!(
            read_facts(facts).unwrap_err().to_string(),
            "/etc/js-agent/flags: not UTF-8 text"
        );
    }

    #[test]
    fn a_file_of_roots_is_trusted_whoever_runs_this() {
        let mut facts = file(ON);
        facts.uid = 0;
        assert!(matches!(read_facts(facts).unwrap(), AgentPf::On(_)));
    }

    #[test]
    fn the_system_reads_a_files_facts_and_does_not_open_a_directory() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-agent-flags-system");
        let path = dir.join("flags");
        fs::write(&path, ON).unwrap();
        let facts = read_system(&path).unwrap();
        assert!(facts.regular);
        assert_eq!(facts.bytes, ON.as_bytes());
        assert_eq!(facts.mode & 0o200, 0o200);
        let facts = read_system(dir.path()).unwrap();
        assert!(!facts.regular);
        assert!(facts.bytes.is_empty());
        let missing = read_system(&dir.join("absent")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
