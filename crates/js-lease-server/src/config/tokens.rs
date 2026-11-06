//! The bearer tokens of the client side: the token of `--token-file`
//! (`server.client_token_file`), which the lease routes take, and the clients of the job
//! service, each a name and a token of its own (`--client-token NAME=PATH`, a `[[client]]`
//! table), among them the token of `--token-file` as the client named `default`.
//!
//! A client is kept as its name and the SHA-256 digest of its token: its file is read, the
//! token checked ([`parse_token`]) and hashed, and the text dropped. A presented token is
//! hashed and its digest compared with every client's in constant time
//! ([`Clients::identify`]): every comparison is made whichever matches, so the time a check
//! takes says neither whether nor which client the token is; and every digest has one
//! length, so neither does the token's length. No `Debug` of this module prints a token or a
//! digest.

use std::fmt;
use std::path::Path;

use js_jobs::ClientName;
use js_registry::Credential;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;

use super::{Cli, MIN_TOKEN_BYTES, read_private_reason};
use crate::error::StartupError;

/// The bearer token. It is compared in constant time and never printed.
#[derive(Clone)]
pub struct Token(pub(super) Vec<u8>);

impl Token {
    /// Whether `given` is the token. The comparison takes the same time for any two
    /// tokens of one length; a token of another length is refused at once (the length
    /// of a token is not a secret).
    pub(crate) fn matches(&self, given: &[u8]) -> bool {
        self.0.ct_eq(given).into()
    }

    /// Whether `other` is the same token, compared as [`Token::matches`] does.
    pub(crate) fn same_as(&self, other: &Token) -> bool {
        self.matches(&other.0)
    }

    /// The credential a host admitted with this token is bound to. A token is printable
    /// ASCII, so its text is exact.
    pub(crate) fn credential(&self) -> Credential {
        Credential::of_token(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The token a file's bytes hold: its content without trailing whitespace, which must
/// be one run of printable ASCII (an HTTP header value cannot carry anything else, so
/// any other token could never match a request) of at least [`MIN_TOKEN_BYTES`] bytes.
pub(crate) fn parse_token(bytes: &[u8]) -> Result<Token, String> {
    let token = bytes.trim_ascii_end();
    let printable = token.iter().all(u8::is_ascii_graphic);
    match (token.is_empty(), printable, token.len() < MIN_TOKEN_BYTES) {
        (true, _, _) => Err("is empty".to_string()),
        (false, false, _) => Err(
            "must hold one token of printable ASCII with no space or line break inside it"
                .to_string(),
        ),
        (false, true, true) => Err(format!(
            "holds a token of {} bytes; a token must have at least {MIN_TOKEN_BYTES} (use a random one)",
            token.len()
        )),
        (false, true, false) => Ok(Token(token.to_vec())),
    }
}

/// The token of the secret file at `path`, read by `read`, or why there is none: the reason
/// names the path and never the content.
pub(crate) fn token_at(
    path: &Path,
    read: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
) -> Result<Token, String> {
    let bytes = read(path)?;
    parse_token(&bytes).map_err(|reason| format!("{} {reason}", path.display()))
}

/// The SHA-256 digest of `token`.
fn digest(token: &[u8]) -> Vec<u8> {
    Sha256::digest(token).to_vec()
}

/// One client: its name, the entry of the configuration that gave it (for the refusals that
/// name it), and the digest of its token.
#[derive(Clone)]
struct Client {
    name: ClientName,
    place: String,
    digest: Vec<u8>,
}

/// The clients of the job service: the client named `default`, whose token is the one of
/// `--token-file` (`server.client_token_file`), and each named client, in the order the
/// configuration gives them. No two have one name or one token. Its `Debug` prints the names
/// only.
#[derive(Clone)]
pub(crate) struct Clients {
    each: Vec<Client>,
}

impl Clients {
    /// The client named `default` alone, whose token is `token`, given by `place` (the
    /// entry a refusal names it by).
    pub(crate) fn only(token: &Token, place: &str) -> Clients {
        let default = Client {
            name: ClientName::default_client(),
            place: place.to_owned(),
            digest: digest(&token.0),
        };
        Clients {
            each: vec![default],
        }
    }

    /// Adds the client `name`, whose token is `token`, given by `place`. Refused when an
    /// earlier client has the name or the token.
    pub(crate) fn add(
        &mut self,
        name: ClientName,
        token: &Token,
        place: String,
    ) -> Result<(), Taken> {
        let digest = digest(&token.0);
        if let Some(other) = self.each.iter().find(|client| client.name == name) {
            return Err(Taken::Name(other.place.clone()));
        }
        if let Some(other) = self.holder(&digest) {
            return Err(Taken::Token(other.to_owned()));
        }
        self.each.push(Client {
            name,
            place,
            digest,
        });
        Ok(())
    }

    /// The entry of the client whose token is `token`, if one's is.
    pub(crate) fn holder_of(&self, token: &Token) -> Option<&str> {
        self.holder(&digest(&token.0))
    }

    /// The entry of the client whose token has `digest`, if one's has.
    fn holder(&self, digest: &[u8]) -> Option<&str> {
        self.each
            .iter()
            .find(|client| bool::from(client.digest.ct_eq(digest)))
            .map(|client| client.place.as_str())
    }

    /// The client whose token `given` is, if it is one's: the digest of `given` is compared
    /// with every client's, in constant time each, whichever matches.
    pub(crate) fn identify(&self, given: &[u8]) -> Option<ClientName> {
        self.identify_with(given, |known, presented| known.ct_eq(presented).into())
    }

    /// [`Clients::identify`] with the comparison of two digests given: the seam a test
    /// counts the comparisons through.
    fn identify_with(
        &self,
        given: &[u8],
        mut same: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Option<ClientName> {
        let presented = digest(given);
        let mut found = None;
        for client in &self.each {
            if same(client.digest.as_slice(), presented.as_slice()) {
                found = Some(client.name.clone());
            }
        }
        found
    }
}

/// Why [`Clients::add`] refused a client: an earlier client, given by the entry each holds,
/// has its name or its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Taken {
    /// The name is the earlier client's.
    Name(String),
    /// The token is the earlier client's.
    Token(String),
}

impl Taken {
    /// The key of a `[[client]]` table at fault: `name` or `token_file`.
    pub(crate) fn key(&self) -> &'static str {
        match self {
            Taken::Name(_) => "name",
            Taken::Token(_) => "token_file",
        }
    }
}

impl fmt::Display for Taken {
    /// What is wrong, naming the earlier client's entry and never the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Taken::Name(other) => write!(f, "also the name of {other}"),
            Taken::Token(other) => write!(
                f,
                "holds the same token as {other}; each client needs its own token"
            ),
        }
    }
}

impl fmt::Debug for Clients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .each
            .iter()
            .map(|client| client.name.as_str())
            .collect();
        f.debug_tuple("Clients").field(&names).finish()
    }
}

/// The flag of a named client.
const FLAG: &str = "--client-token";

/// What an entry of `--client-token` must be.
const ENTRY_MUST: &str =
    "must be NAME=PATH, with NAME 1 to 40 bytes of letters, digits, '.', '_' and '-'";

/// The clients the flags give: the client named `default`, whose token is `default` (the
/// token of `--token-file`), and one per `--client-token NAME=PATH`, in order, each read from
/// its file with the checks of `--token-file`. An entry that is not `NAME=PATH` with a valid name is
/// refused by its position, without repeating it (a token pasted in its place is never
/// printed); any other refusal names the entry by its name. Named clients need the job
/// service, which alone takes them: without `--job-listen` the flag is refused.
pub(crate) fn from_flags(default: &Token, cli: &Cli) -> Result<Clients, StartupError> {
    let mut clients = Clients::only(default, "the client of --token-file");
    if !cli.client_token.is_empty() && cli.job_listen.is_none() {
        return Err(StartupError::new(FLAG, "requires --job-listen"));
    }
    for (at, entry) in cli.client_token.iter().enumerate() {
        let parsed = entry
            .split_once('=')
            .and_then(|(name, path)| Some((ClientName::parse(name).ok()?, Path::new(path))));
        let Some((name, path)) = parsed else {
            let reason = format!("entry {}: {ENTRY_MUST}", at + 1);
            return Err(StartupError::new(FLAG, reason));
        };
        let named = |reason: String| StartupError::new(FLAG, format!("{name}: {reason}"));
        let token = token_at(path, read_private_reason).map_err(named)?;
        let place = format!("--client-token {name}");
        let added = clients.add(name.clone(), &token, place);
        added.map_err(|taken| named(taken.to_string()))?;
    }
    Ok(clients)
}

#[cfg(test)]
#[path = "tokens_tests.rs"]
mod tests;
