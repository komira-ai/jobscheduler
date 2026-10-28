//! The cloud-init user data that turns a launched Lambda GPU instance into a `js-agent`
//! host.
//!
//! [`Bootstrap::render`] is a pure function of its inputs: it reads no file, makes no
//! call and fetches nothing. The script it renders downloads the agent from the URL the
//! operator gave, refuses it unless its SHA-256 is the one the operator gave, checks that
//! the image carries the driver, the container engine and the NVIDIA container toolkit,
//! makes the agent's directories and writes a systemd service and a path unit. The
//! service is gated on the token file and the CA file, so the agent starts only once a
//! token has been delivered, by a later step, into the token file.
//!
//! The document holds no secret: no token, no key. The launch API shows the user data to
//! the provider's staff, and a join token has no place in it. Every input is checked so
//! that none can be a token either: a value is a short run of letters, digits and `.`,
//! `_`, `-` (and `/` or `:` where a path or a URL needs them), no run of 32 letters or
//! digits is accepted, and a URL may carry no user, no query and no fragment.

use std::fmt;

use sha2::{Digest, Sha256};

/// The script, with `@NAME@` where an input goes.
const TEMPLATE: &str = include_str!("bootstrap.sh.in");

/// The scratch file system's size when the inputs give none, in GiB. Lambda's smaller GPU
/// instances have a 512 GiB disk: 200 GiB for job scratch leaves the rest for the system,
/// the container images and the state root, and the script refuses to start when less
/// than this size plus 50 GiB is free, so a disk that is too small fails loudly.
pub const DEFAULT_SCRATCH_GIB: u32 = 200;

/// The smallest scratch file system, in GiB.
pub const MIN_SCRATCH_GIB: u32 = 10;

/// The largest scratch file system, in GiB.
pub const MAX_SCRATCH_GIB: u32 = 4096;

/// The longest a value may be.
const MAX_FIELD: usize = 256;

/// A run of this many letters or digits looks like a token and is refused.
const TOKEN_RUN: usize = 32;

/// The inputs of the bootstrap. Every value is operator-provided; nothing is fetched or
/// guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bootstrap<'a> {
    /// Where to download the `js-agent` executable: `https://` only.
    pub agent_url: &'a str,
    /// The SHA-256 the download must have: 64 lowercase hex digits.
    pub agent_sha256: &'a str,
    /// The lease server's URL, `https://` only.
    pub server_url: &'a str,
    /// The pool the agent joins.
    pub pool: &'a str,
    /// The PEM file with the certificate the agent trusts for the server. It is public.
    pub ca_cert: &'a str,
    /// The file the join token is delivered into, later. The unit waits for it.
    pub token_file: &'a str,
    /// The agent's state root.
    pub state_root: &'a str,
    /// The agent's scratch root: the mount point of a file system of its own.
    pub scratch_root: &'a str,
    /// The size of that file system in GiB, from [`MIN_SCRATCH_GIB`] to
    /// [`MAX_SCRATCH_GIB`]; `None` is [`DEFAULT_SCRATCH_GIB`]. It is an ext4 image on the
    /// instance disk, allocated in full, mounted `nodev,nosuid,noatime`.
    pub scratch_gib: Option<u32>,
    /// The container engine's absolute path.
    pub container_bin: &'a str,
}

/// Rendered cloud-init user data. Its `Debug` shows the size and a short digest, never
/// the text, so a log line cannot leak what a later change puts into it.
#[derive(Clone, PartialEq, Eq)]
pub struct UserData(String);

impl UserData {
    /// The document as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 8 bytes of the document's SHA-256, in hex: what a log may say about it.
    pub fn digest(&self) -> String {
        Sha256::digest(self.0.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl fmt::Debug for UserData {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "UserData({} bytes, sha256 {})",
            self.0.len(),
            self.digest()
        )
    }
}

/// What is wrong with one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// The value is empty.
    #[error("is empty")]
    Empty,
    /// The value is longer than 256 bytes.
    #[error("is longer than 256 bytes")]
    TooLong,
    /// The value holds a character outside the allowed few: nothing a shell or systemd
    /// would read as syntax is accepted.
    #[error("holds a character that is not allowed")]
    Character,
    /// The value holds a run of 32 or more letters and digits, which is what a token
    /// looks like.
    #[error("holds a run of 32 or more letters and digits, which looks like a token")]
    TokenShaped,
    /// A URL that does not start with `https://`.
    #[error("must be an https:// URL")]
    NotHttps,
    /// A URL with no host.
    #[error("has no host")]
    NoHost,
    /// A path that is not absolute.
    #[error("must be an absolute path")]
    NotAbsolute,
    /// A path with an empty, `.` or `..` component, or the root or a trailing `/`.
    #[error("has an empty, '.' or '..' component")]
    Component,
    /// A SHA-256 that is not 64 lowercase hex digits.
    #[error("must be 64 lowercase hex digits")]
    Digest,
}

/// Why the user data cannot be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BootstrapError {
    /// One input is refused.
    #[error("{field}: {fault}")]
    Field {
        /// The input.
        field: &'static str,
        /// What is wrong with it.
        fault: Fault,
    },
    /// The scratch size is outside the bounds.
    #[error(
        "scratch_gib: {gib} is outside {} to {} GiB",
        MIN_SCRATCH_GIB,
        MAX_SCRATCH_GIB
    )]
    ScratchSize {
        /// The size asked for.
        gib: u32,
    },
    /// Two paths are, or lie in, one another.
    #[error("{first} and {second} are the same path or one lies in the other")]
    Overlap {
        /// One input.
        first: &'static str,
        /// The other.
        second: &'static str,
    },
}

impl Bootstrap<'_> {
    /// The user data for these inputs.
    ///
    /// # Errors
    ///
    /// [`BootstrapError`] for the first input that is not an https URL, a lowercase
    /// SHA-256, an absolute path or a plain name as described on [`Bootstrap`]; for a
    /// token-shaped run in any of them; and for a token file inside the state or scratch
    /// root, or a state root and scratch root that overlap.
    pub fn render(&self) -> Result<UserData, BootstrapError> {
        url("agent_url", self.agent_url)?;
        digest("agent_sha256", self.agent_sha256)?;
        url("server_url", self.server_url)?;
        plain("pool", self.pool, "")?;
        path("ca_cert", self.ca_cert)?;
        path("token_file", self.token_file)?;
        path("state_root", self.state_root)?;
        path("scratch_root", self.scratch_root)?;
        path("container_bin", self.container_bin)?;
        let gib = self.scratch_gib.unwrap_or(DEFAULT_SCRATCH_GIB);
        if !(MIN_SCRATCH_GIB..=MAX_SCRATCH_GIB).contains(&gib) {
            return Err(BootstrapError::ScratchSize { gib });
        }
        if lies_in(self.state_root, self.scratch_root)
            || lies_in(self.scratch_root, self.state_root)
        {
            return Err(BootstrapError::Overlap {
                first: "state_root",
                second: "scratch_root",
            });
        }
        if lies_in(self.token_file, self.state_root) {
            return Err(BootstrapError::Overlap {
                first: "token_file",
                second: "state_root",
            });
        }
        if lies_in(self.token_file, self.scratch_root) {
            return Err(BootstrapError::Overlap {
                first: "token_file",
                second: "scratch_root",
            });
        }
        let text = TEMPLATE
            .replace("@AGENT_URL@", self.agent_url)
            .replace("@AGENT_SHA256@", self.agent_sha256)
            .replace("@SERVER_URL@", self.server_url)
            .replace("@POOL@", self.pool)
            .replace("@CA_CERT@", self.ca_cert)
            .replace("@TOKEN_FILE@", self.token_file)
            .replace("@STATE_ROOT@", self.state_root)
            .replace("@SCRATCH_ROOT@", self.scratch_root)
            .replace("@SCRATCH_GIB@", &gib.to_string())
            .replace("@CONTAINER_BIN@", self.container_bin);
        Ok(UserData(text))
    }
}

fn refuse(field: &'static str, fault: Fault) -> BootstrapError {
    BootstrapError::Field { field, fault }
}

/// Letters, digits and `.`, `_`, `-`, plus the bytes of `extra`; not empty, not long, no
/// token-shaped run.
fn plain(field: &'static str, text: &str, extra: &str) -> Result<(), BootstrapError> {
    if text.is_empty() {
        return Err(refuse(field, Fault::Empty));
    }
    if text.len() > MAX_FIELD {
        return Err(refuse(field, Fault::TooLong));
    }
    let allowed =
        |b: u8| b.is_ascii_alphanumeric() || b".-_".contains(&b) || extra.as_bytes().contains(&b);
    if !text.bytes().all(allowed) {
        return Err(refuse(field, Fault::Character));
    }
    if text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| part.len() >= TOKEN_RUN)
    {
        return Err(refuse(field, Fault::TokenShaped));
    }
    Ok(())
}

fn url(field: &'static str, text: &str) -> Result<(), BootstrapError> {
    plain(field, text, "/:")?;
    let Some(rest) = text.strip_prefix("https://") else {
        return Err(refuse(field, Fault::NotHttps));
    };
    match rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        true => Ok(()),
        false => Err(refuse(field, Fault::NoHost)),
    }
}

fn path(field: &'static str, text: &str) -> Result<(), BootstrapError> {
    plain(field, text, "/")?;
    if !text.starts_with('/') {
        return Err(refuse(field, Fault::NotAbsolute));
    }
    match text
        .split('/')
        .skip(1)
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        true => Err(refuse(field, Fault::Component)),
        false => Ok(()),
    }
}

fn digest(field: &'static str, text: &str) -> Result<(), BootstrapError> {
    match text.len() == 64 && text.bytes().all(is_lower_hex) {
        true => Ok(()),
        false => Err(refuse(field, Fault::Digest)),
    }
}

fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Whether `inner` is `outer` or lies under it (both validated absolute paths).
fn lies_in(inner: &str, outer: &str) -> bool {
    inner == outer
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}
