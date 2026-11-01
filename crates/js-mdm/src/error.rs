//! Why a verb refused or failed.

use std::io;

use js_host_step::ExecError;
use js_mdm_core::config::ConfigError;
use js_mdm_core::path::PathError;
use js_mdm_core::template::TemplateError;

use crate::archive::ArchiveError;

/// Why a verb refused (exit 2: fix the input or the host) or failed (exit 1: a step, a
/// download or a check did not work).
#[derive(Debug, thiserror::Error)]
pub enum MdmError {
    /// The settings file was refused.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A unit template did not render.
    #[error(transparent)]
    Template(#[from] TemplateError),
    /// A path is not an absolute path.
    #[error(transparent)]
    Path(#[from] PathError),
    /// An archive could not be read.
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    /// A plan could not be printed or carried out.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// A file or program of the host could not be reached.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// A file that must be there is not.
    #[error("{0} is not there")]
    NoFile(String),
    /// `fetch` was run as root.
    #[error("run fetch as yourself, not root: downloads never run as root")]
    FetchAsRoot,
    /// `--dir` is inside a git work tree.
    #[error(
        "--dir {dir} is inside a git work tree ({git} exists): keep archives and keys outside every repository"
    )]
    InGitTree {
        /// The directory.
        dir: String,
        /// The `.git` found.
        git: String,
    },
    /// A kept archive is not the pin.
    #[error("{file}: sha256 {got} is not the pin {want}: delete it and re-run")]
    KeptDiffers {
        /// The file.
        file: String,
        /// Its sha256.
        got: String,
        /// The pin's.
        want: String,
    },
    /// A download is not the pin.
    #[error(
        "{tool}: the download's sha256 {got} is not the pin {want} (kept as {kept} for inspection)"
    )]
    Rejected {
        /// The tool.
        tool: String,
        /// The download's sha256.
        got: String,
        /// The pin's.
        want: String,
        /// Where the download was kept.
        kept: String,
    },
    /// A download failed.
    #[error("download of {url} failed: {why}")]
    Download {
        /// The URL.
        url: String,
        /// Why.
        why: String,
    },
    /// A verb that reads root-only files was run by someone else.
    #[error(
        "{verb} reads root-only files: run it as root, even for a dry run (effective uid is {euid})"
    )]
    NeedsRoot {
        /// The verb.
        verb: &'static str,
        /// The effective uid.
        euid: u32,
    },
    /// An archive `fetch` should have downloaded is missing.
    #[error("{0} is missing (js-mdm fetch --apply downloads it)")]
    NoArchive(String),
    /// An archive is not its pin.
    #[error("{path}: sha256 {got} is not the pin {want}")]
    ArchiveDiffers {
        /// The file.
        path: String,
        /// Its sha256.
        got: String,
        /// The pin's.
        want: String,
    },
    /// A directory the kit owns is a symbolic link.
    #[error("{0} is a symbolic link: name the real directory")]
    IsLink(String),
    /// An archive is larger than the cap.
    #[error("{path} is larger than the {max} bytes an archive may be")]
    ArchiveTooBig {
        /// The file.
        path: String,
        /// The cap.
        max: u64,
    },
    /// A secret file would lose other lines when it is made again.
    #[error(
        "{path} has no value for {key} but holds other lines: remove them or give {key} a value, and run again"
    )]
    SecretOther {
        /// The file.
        path: String,
        /// The key.
        key: String,
    },
    /// An `--ignore` value that cannot be one whole line.
    #[error("--ignore {0:?} must be one non-empty line")]
    BadIgnore(String),
    /// The bind address is on no interface.
    #[error("no interface carries MDM_BIND={0} (ip -4 addr)")]
    NotCarried(String),
    /// A port is held by something that is not the kit.
    #[error("port {0} is held by another process (ss -ltnp 'sport = :{0}')")]
    PortHeld(u16),
    /// A secret file is not a 0600 file of root's.
    #[error(
        "{path} has mode {mode:04o} and uid {uid}, not 0600 and root: chmod 600 {path}; chown root:root {path}"
    )]
    SecretMode {
        /// The file.
        path: String,
        /// Its mode.
        mode: u32,
        /// Its owner.
        uid: u32,
    },
    /// A secret file lacks a value.
    #[error("{path} has no {key}= line with a value")]
    NoSecret {
        /// The file.
        path: String,
        /// The key.
        key: String,
    },
    /// A secret value holds a control character.
    #[error("{path}: the value of {key} holds a control character")]
    SecretValue {
        /// The file.
        path: String,
        /// The key.
        key: String,
    },
    /// lego is not installed.
    #[error("{0} is missing: js-mdm install --apply installs it")]
    NoLego(String),
    /// lego lacks a flag the certificate unit passes.
    #[error("{lego} run --help lists no {flag}: not the lego the pins name")]
    LegoFlag {
        /// The binary.
        lego: String,
        /// The flag.
        flag: &'static str,
    },
    /// A program the verb relies on failed.
    #[error("{line} exited with status {code}")]
    Command {
        /// The command line.
        line: String,
        /// The status.
        code: i32,
    },
    /// There is no certificate after the issuing unit ran.
    #[error("no certificate at {0} after the certificate unit ran: journalctl -u on its unit")]
    NoCertificate(String),
    /// The certificate file is not one openssl reads.
    #[error("{0} is there and is not a PEM certificate")]
    NotCertificate(String),
    /// A service's address is not loopback.
    #[error("{0} is not a loopback address: the API keys never leave the host")]
    NotLoopback(String),
    /// The running binary is one others could replace.
    #[error(
        "{0}: the API calls carry keys, so they are made only by a binary root owns alone (chown root and chmod go-w it)"
    )]
    Untrusted(String),
    /// The listener on a service's port is not the service's.
    #[error(
        "port {port} on loopback is held by uid {uid}, not root or the service user: the API key is not sent (ss -ltnp 'sport = :{port}')"
    )]
    Squatter {
        /// The port.
        port: u16,
        /// The owner of the listener.
        uid: u32,
    },
    /// The recorded profile uuid is not one.
    #[error(
        "{0} does not hold a profile uuid of 16 to 64 hex digits: run js-mdm dep define-profile --apply --replace"
    )]
    BadRecord(String),
    /// A call to a service failed or was refused by it.
    #[error("{call}: {why}")]
    Api {
        /// The call, as it is printed.
        call: String,
        /// Why it failed.
        why: String,
    },
    /// A service's answer is not what the verb needs.
    #[error("{what}: {why}")]
    Answer {
        /// What the answer was for.
        what: &'static str,
        /// Why it is not usable.
        why: String,
    },
    /// No ADE profile has been defined.
    #[error(
        "no ADE profile is recorded ({0} is not there): run js-mdm dep define-profile --apply first"
    )]
    NoProfile(String),
    /// The recorded ADE profile is not this render.
    #[error(
        "an ADE profile is already defined (uuid {uuid}) from a render with sha256 {recorded:?}, and this render has sha256 {rendered}: if the MDM_ADE_* settings should differ, define a second profile with --replace; otherwise set them to the recorded profile's values"
    )]
    ProfileDiffers {
        /// The recorded profile's uuid.
        uuid: String,
        /// The sha256 recorded with it (empty when none is).
        recorded: String,
        /// The sha256 of this render.
        rendered: String,
    },
}

impl MdmError {
    /// The exit code: 1 when a step, a download or a program failed, 2 when the input or
    /// the host was refused.
    pub fn exit_code(&self) -> u8 {
        match self {
            MdmError::Exec(ExecError::Step { .. } | ExecError::Write(_))
            | MdmError::Io(_)
            | MdmError::Rejected { .. }
            | MdmError::Download { .. }
            | MdmError::Api { .. }
            | MdmError::Answer { .. }
            | MdmError::Command { .. }
            | MdmError::NoCertificate(_)
            | MdmError::NotCertificate(_) => 1,
            _ => 2,
        }
    }
}
