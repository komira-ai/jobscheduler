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
//!
//! # The pre-start fragment
//!
//! Agents run on many networks, and some need a step that joins one first (a VPN, an
//! overlay, a tunnel). Nothing of the kind is built in: the contract per launch is a
//! scheduler URL, an optional CA bundle and the one-time join token. An operator who
//! needs such a step supplies it as [`Bootstrap::prestart`], shell text that is empty by
//! default.
//!
//! When it is empty the document has no pre-start unit and `js-agent.service` is exactly
//! the unit described above. When it is set the bootstrap writes the text to
//! `/usr/local/lib/js-agent/prestart.sh` and installs `js-agent-prestart.service`
//! (`Type=oneshot`, `RemainAfterExit=yes`), which runs the text with `bash -e -o pipefail`
//! after `network-online.target` and after the scratch mount. The bootstrap runs it once
//! before it finishes, and a failure fails the bootstrap; `js-agent.service` gets
//! `Requires=` and `After=` on it, so the agent never starts when it fails.
//!
//! The text never receives the join token: the token arrives later, over ssh, into the
//! token file, and the bootstrap exports nothing to the fragment. The text is part of the
//! user data, which the launch API shows to the provider's staff, so it holds no secret
//! either: a key or a config with a secret belongs on the image, and the fragment names
//! its path. The text is checked: at most [`MAX_PRESTART_BYTES`] bytes, no NUL or other
//! control character (a tab and newlines excepted), no run of 32 or more letters and
//! digits, and not the delimiter of the heredoc that carries it. It is written through a
//! quoted heredoc, so `$`, backticks, quotes and backslashes reach the file unchanged.
//!
//! An example, for an operator whose instances reach the scheduler through a WireGuard
//! interface whose config is on the image (an example only; any step fits):
//!
//! ```text
//! wg-quick up wg-agent
//! ```

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

/// The longest pre-start fragment, in bytes. The text is operator-written glue, a few
/// commands; a larger one belongs on the image as a file the fragment runs.
pub const MAX_PRESTART_BYTES: usize = 4096;

/// The heredoc delimiter that carries the fragment; a fragment holding it is refused.
const PRESTART_DELIMITER: &str = "JS_AGENT_PRESTART_END";

/// The unit and script block that installs the fragment, with `@PRESTART_FRAGMENT@`.
const PRESTART_UNIT: &str = include_str!("bootstrap_prestart.sh.in");

/// The step that runs the fragment once, after the scratch mount is checked.
const PRESTART_START: &str = "systemctl enable --now js-agent-prestart.service ||\n    fail \"the pre-start fragment failed: see journalctl -u js-agent-prestart.service\"";

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
    /// The operator's pre-start fragment: shell text, at most [`MAX_PRESTART_BYTES`]
    /// bytes, run before the agent starts and after the network and the scratch mount
    /// are up. Empty (the default) means no such step. See the module docs for what is
    /// checked and what the fragment does not receive (the join token).
    pub prestart: &'a str,
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
    /// A pre-start fragment larger than [`MAX_PRESTART_BYTES`].
    #[error("is larger than {} bytes", MAX_PRESTART_BYTES)]
    TooLarge,
    /// A pre-start fragment with a NUL byte.
    #[error("holds a NUL byte")]
    Nul,
    /// A pre-start fragment with a control character other than a tab or a newline.
    #[error("holds a control character other than a tab or a newline")]
    Control,
    /// A pre-start fragment that holds the delimiter of the heredoc carrying it.
    #[error("holds the heredoc delimiter {}", PRESTART_DELIMITER)]
    Delimiter,
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
        if !self.prestart.is_empty() {
            prestart("prestart", self.prestart)?;
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
        // The fragment goes in last, so no later replacement can touch its text.
        let text = if self.prestart.is_empty() {
            text.replace("@PRESTART_AFTER@", "")
                .replace("@PRESTART_REQUIRES@\n", "")
                .replace("@PRESTART_UNIT@\n", "")
                .replace("@PRESTART_START@\n", "")
        } else {
            let fragment = self.prestart.trim_end_matches('\n');
            text.replace("@PRESTART_AFTER@", " js-agent-prestart.service")
                .replace("@PRESTART_REQUIRES@", "Requires=js-agent-prestart.service")
                .replace("@PRESTART_START@", PRESTART_START)
                .replace(
                    "@PRESTART_UNIT@\n",
                    &PRESTART_UNIT.replace("@PRESTART_FRAGMENT@", fragment),
                )
        };
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

/// The fragment: not blank, not large, no NUL or control character, not the heredoc
/// delimiter, no token-shaped run.
fn prestart(field: &'static str, text: &str) -> Result<(), BootstrapError> {
    if text.trim().is_empty() {
        return Err(refuse(field, Fault::Empty));
    }
    if text.len() > MAX_PRESTART_BYTES {
        return Err(refuse(field, Fault::TooLarge));
    }
    if text.contains('\0') {
        return Err(refuse(field, Fault::Nul));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(refuse(field, Fault::Control));
    }
    if text.contains(PRESTART_DELIMITER) {
        return Err(refuse(field, Fault::Delimiter));
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
