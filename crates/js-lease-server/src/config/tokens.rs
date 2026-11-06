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
//!
//! Each client has a role ([`Role`]): `client`, the default of a named client, or `operator`,
//! which `default` always has. The set is held behind one shared handle ([`LiveClients`]):
//! the bearer layer reads the set in force at each request, and [`LiveClients::reload`] reads
//! every named client's token file again and replaces the set only when every file is still
//! valid. The names, roles and paths are the configuration's, read once at start; the token
//! of `default` is the lease routes' token, read once at start with them.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

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

/// What a client may call on the job service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Submits jobs, and reads and cancels only the jobs it submitted; no fleet call. The
    /// role of a named client whose entry gives none.
    Client,
    /// Every call, on every job: the fleet calls, and the reads and cancels of any job. The
    /// role of `default`, whose token is the lease routes'.
    Operator,
}

impl Role {
    /// What an entry's role must be, for the refusals.
    pub(crate) const MUST: &'static str = "must be \"client\" or \"operator\"";

    /// The role `text` names: `client` or `operator`, exactly; `None` for anything else.
    pub(crate) fn parse(text: &str) -> Option<Role> {
        match text {
            "client" => Some(Role::Client),
            "operator" => Some(Role::Operator),
            _ => None,
        }
    }

    /// The role's name, as an entry gives it and a log line says it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Operator => "operator",
        }
    }
}

/// One client: its name, its role, the entry of the configuration that gave it (for the
/// refusals that name it), the digest of its token, and the file its token is read from
/// again (`None` for `default`, whose token is read once at start).
#[derive(Clone)]
struct Client {
    name: ClientName,
    role: Role,
    place: String,
    digest: Vec<u8>,
    file: Option<PathBuf>,
}

/// The clients of the job service: the client named `default`, whose token is the one of
/// `--token-file` (`server.client_token_file`), and each named client, in the order the
/// configuration gives them. No two have one name or one token. Its `Debug` prints the names
/// and roles only.
#[derive(Clone)]
pub(crate) struct Clients {
    each: Vec<Client>,
}

impl Clients {
    /// The client named `default` alone, an operator, whose token is `token`, given by
    /// `place` (the entry a refusal names it by).
    pub(crate) fn only(token: &Token, place: &str) -> Clients {
        let default = Client {
            name: ClientName::default_client(),
            role: Role::Operator,
            place: place.to_owned(),
            digest: digest(&token.0),
            file: None,
        };
        Clients {
            each: vec![default],
        }
    }

    /// Adds the client `entry` names, whose token is `token`, read from the entry's file.
    /// Refused when an earlier client has the name or the token.
    pub(crate) fn add(&mut self, entry: Entry, token: &Token) -> Result<(), Taken> {
        let digest = digest(&token.0);
        if let Some(other) = self.each.iter().find(|client| client.name == entry.name) {
            return Err(Taken::Name(other.place.clone()));
        }
        if let Some(other) = self.holder(&digest) {
            return Err(Taken::Token(other.to_owned()));
        }
        self.each.push(Client {
            name: entry.name,
            role: entry.role,
            place: entry.place,
            digest,
            file: Some(entry.file),
        });
        Ok(())
    }

    /// The same clients, each named client's token read again from its file by `read`:
    /// `default` and every name, role and order kept. Refused, naming the client's entry and
    /// never the token, when a file cannot be read or holds no valid token, when two clients
    /// would hold one token, or when `taken` says a token is not a client's to hold (it
    /// gives the reason); nothing is replaced then.
    pub(crate) fn reread(
        &self,
        read: &dyn Fn(&Path) -> Result<Vec<u8>, String>,
        taken: &dyn Fn(&Token) -> Option<String>,
    ) -> Result<Clients, String> {
        let mut again = Clients {
            each: Vec::with_capacity(self.each.len()),
        };
        for client in &self.each {
            let Some(file) = &client.file else {
                again.each.push(client.clone());
                continue;
            };
            let place = &client.place;
            let token = token_at(file, read).map_err(|reason| format!("{place}: {reason}"))?;
            if let Some(reason) = taken(&token) {
                return Err(format!("{place}: {reason}"));
            }
            let entry = Entry {
                name: client.name.clone(),
                role: client.role,
                place: place.clone(),
                file: file.clone(),
            };
            again
                .add(entry, &token)
                .map_err(|taken| format!("{place}: {taken}"))?;
        }
        Ok(again)
    }

    /// Whether `other` holds the same tokens, in the same order: of a set and its
    /// [`Clients::reread`], which keeps every name, role and place, the tokens alone can
    /// differ.
    fn same_as(&self, other: &Clients) -> bool {
        let digests = |clients: &Clients| -> Vec<Vec<u8>> {
            clients
                .each
                .iter()
                .map(|client| client.digest.clone())
                .collect()
        };
        digests(self) == digests(other)
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

    /// The client whose token `given` is, with its role, if it is one's: the digest of
    /// `given` is compared with every client's, in constant time each, whichever matches.
    pub(crate) fn identify(&self, given: &[u8]) -> Option<(ClientName, Role)> {
        self.identify_with(given, |known, presented| known.ct_eq(presented).into())
    }

    /// [`Clients::identify`] with the comparison of two digests given: the seam a test
    /// counts the comparisons through.
    fn identify_with(
        &self,
        given: &[u8],
        mut same: impl FnMut(&[u8], &[u8]) -> bool,
    ) -> Option<(ClientName, Role)> {
        let presented = digest(given);
        let mut found = None;
        for client in &self.each {
            if same(client.digest.as_slice(), presented.as_slice()) {
                found = Some((client.name.clone(), client.role));
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
        let names: Vec<(&str, &str)> = self
            .each
            .iter()
            .map(|client| (client.name.as_str(), client.role.as_str()))
            .collect();
        f.debug_tuple("Clients").field(&names).finish()
    }
}

/// A named client as its entry of the configuration gives it: the name, the role, the entry
/// a refusal names it by, and the file of its token.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The client's name.
    pub(crate) name: ClientName,
    /// What it may call.
    pub(crate) role: Role,
    /// The entry of the configuration, as a refusal names it.
    pub(crate) place: String,
    /// The file its token is read from, at start and at every reload.
    pub(crate) file: PathBuf,
}

/// The clients in force, behind one handle every holder shares: the bearer layer reads the
/// set at each request ([`LiveClients::now`]), so a reload takes effect from the next
/// request. Its `Debug` is the set's.
#[derive(Clone)]
pub(crate) struct LiveClients(Arc<RwLock<Arc<Clients>>>);

impl LiveClients {
    /// The handle on `clients`.
    pub(crate) fn new(clients: Clients) -> LiveClients {
        LiveClients(Arc::new(RwLock::new(Arc::new(clients))))
    }

    /// The set in force.
    pub(crate) fn now(&self) -> Arc<Clients> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Reads every named client's token file again by `read` ([`Clients::reread`], with
    /// `taken` the tokens no client may hold) and, when every file is valid, puts the new
    /// set in force; whether its tokens differ from the set it replaced. A refusal keeps the
    /// set in force, whole, and says why, naming the entry and never the token.
    pub(crate) fn reload(
        &self,
        read: &dyn Fn(&Path) -> Result<Vec<u8>, String>,
        taken: &dyn Fn(&Token) -> Option<String>,
    ) -> Result<bool, String> {
        let again = self.now().reread(read, taken)?;
        let mut held = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let changed = !held.same_as(&again);
        *held = Arc::new(again);
        Ok(changed)
    }
}

impl fmt::Debug for LiveClients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.now().fmt(f)
    }
}

/// The flag of a named client.
const FLAG: &str = "--client-token";

/// What an entry of `--client-token` must be.
const ENTRY_MUST: &str = "must be NAME=PATH or NAME:ROLE=PATH, with NAME 1 to 40 bytes of \
     letters, digits, '.', '_' and '-' and ROLE \"client\" or \"operator\"";

/// The name, role and path an entry of `--client-token` gives: `NAME=PATH` (the role
/// `client`) or `NAME:ROLE=PATH`. A name holds no `:`, so the first `:` before the `=` ends
/// it.
fn entry_of(entry: &str) -> Option<(ClientName, Role, &Path)> {
    let (head, path) = entry.split_once('=')?;
    let (name, role) = match head.split_once(':') {
        Some((name, role)) => (name, Role::parse(role)?),
        None => (head, Role::Client),
    };
    Some((ClientName::parse(name).ok()?, role, Path::new(path)))
}

/// The clients the flags give: the client named `default`, whose token is `default` (the
/// token of `--token-file`), and one per `--client-token NAME[:ROLE]=PATH`, in order, each
/// read from its file with the checks of `--token-file`. An entry that is not of that form,
/// with a valid name and role, is refused by its position, without repeating it (a token
/// pasted in its place is never printed); any other refusal names the entry by its name.
/// Named clients need the job service, which alone takes them: without `--job-listen` the
/// flag is refused.
pub(crate) fn from_flags(default: &Token, cli: &Cli) -> Result<Clients, StartupError> {
    let mut clients = Clients::only(default, "the client of --token-file");
    if !cli.client_token.is_empty() && cli.job_listen.is_none() {
        return Err(StartupError::new(FLAG, "requires --job-listen"));
    }
    for (at, entry) in cli.client_token.iter().enumerate() {
        let Some((name, role, path)) = entry_of(entry) else {
            let reason = format!("entry {}: {ENTRY_MUST}", at + 1);
            return Err(StartupError::new(FLAG, reason));
        };
        let named = |reason: String| StartupError::new(FLAG, format!("{name}: {reason}"));
        let token = token_at(path, read_private_reason).map_err(named)?;
        let entry = Entry {
            name: name.clone(),
            role,
            place: format!("--client-token {name}"),
            file: path.to_path_buf(),
        };
        let added = clients.add(entry, &token);
        added.map_err(|taken| named(taken.to_string()))?;
    }
    Ok(clients)
}

#[cfg(test)]
#[path = "tokens_tests.rs"]
mod tests;
