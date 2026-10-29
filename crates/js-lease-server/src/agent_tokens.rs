//! The join tokens of `--agent-token-file`: several at once, so that one can be replaced
//! while hosts still use the other, and read again at every tick.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use js_registry::Credential;

use crate::config::{Token, parse_token, read_private};
use crate::error::StartupError;

/// The most tokens the file may hold. A request is compared against each of them, so the
/// count is bounded.
pub(crate) const MAX_AGENT_TOKENS: usize = 16;

/// The flag the file comes from, for the refusals.
const FLAG: &str = "--agent-token-file";

/// One accepted token and the credential a host admitted with it is bound to.
struct Accepted {
    token: Token,
    credential: Credential,
}

/// The join tokens the server accepts from agents.
///
/// The set is read from the file at start and again by [`AgentTokens::reload`]. A read
/// that fails keeps the set in force, and the caller logs it: an operator's half-saved
/// edit must not lock every agent out, and must not go unnoticed either.
pub(crate) struct AgentTokens {
    path: Option<PathBuf>,
    client: Token,
    accepted: RwLock<Arc<Vec<Accepted>>>,
}

impl AgentTokens {
    /// Reads the file at `path`. `client` is the token of the lease routes: the two
    /// kinds of token must differ, or the join token would open the lease routes' side
    /// of the server's trust and the other way round.
    pub(crate) fn open(path: &Path, client: &Token) -> Result<Self, StartupError> {
        let accepted = read(path, client)?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            client: client.clone(),
            accepted: RwLock::new(Arc::new(accepted)),
        })
    }

    /// No shared join tokens: no host is admitted with a token every host of the pool
    /// shares. A pool without a join token file admits only hosts that present a one-time
    /// token or a host credential the server minted.
    pub(crate) fn none(client: &Token) -> Self {
        Self {
            path: None,
            client: client.clone(),
            accepted: RwLock::new(Arc::new(Vec::new())),
        }
    }

    /// Reads the file again and, when it is valid, replaces the set. The file's text is
    /// never in the error. Without a file there is nothing to read.
    pub(crate) fn reload(&self) -> Result<(), StartupError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let accepted = read(path, &self.client)?;
        *self
            .accepted
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(accepted);
        Ok(())
    }

    /// The credentials of the tokens in force.
    pub(crate) fn credentials(&self) -> Vec<Credential> {
        let accepted = self.accepted.read().unwrap_or_else(PoisonError::into_inner);
        accepted
            .iter()
            .map(|each| each.credential.clone())
            .collect()
    }

    /// The credential of the accepted token that `given` is, if it is one. Every accepted
    /// token is compared, in constant time each, whichever matches first.
    pub(crate) fn identify(&self, given: &str) -> Option<Credential> {
        let accepted = self
            .accepted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut found = None;
        for each in accepted.iter() {
            if each.token.matches(given.as_bytes()) {
                found = Some(each.credential.clone());
            }
        }
        found
    }
}

/// The tokens a file's bytes hold, one per line.
fn parse(bytes: &[u8]) -> Result<Vec<Token>, String> {
    let mut tokens: Vec<Token> = Vec::new();
    for (number, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.trim_ascii().is_empty() {
            continue;
        }
        let token = parse_token(line).map_err(|reason| format!("line {}: {reason}", number + 1))?;
        if !tokens.iter().any(|seen| seen.same_as(&token)) {
            tokens.push(token);
        }
    }
    match (tokens.is_empty(), tokens.len() > MAX_AGENT_TOKENS) {
        (true, _) => Err("holds no token".to_string()),
        (false, true) => Err(format!("holds more than {MAX_AGENT_TOKENS} tokens")),
        (false, false) => Ok(tokens),
    }
}

/// Reads and checks the file: owner-only like the client token file, every line a token
/// of at least [`crate::MIN_TOKEN_BYTES`], none the client token.
fn read(path: &Path, client: &Token) -> Result<Vec<Accepted>, StartupError> {
    let bytes = read_private(path, FLAG)?;
    let named = |reason: String| StartupError::new(FLAG, format!("{} {reason}", path.display()));
    let tokens = parse(&bytes).map_err(named)?;
    if tokens.iter().any(|token| token.same_as(client)) {
        return Err(named(
            "holds the client token (--token-file): the two must differ".to_string(),
        ));
    }
    Ok(tokens
        .into_iter()
        .map(|token| Accepted {
            credential: token.credential(),
            token,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "agent-token-A-0123456789-abcdefghijklmn";
    const B: &str = "agent-token-B-0123456789-abcdefghijklmn";

    /// Lines are tokens: blank lines and a CR before the line break are dropped, a repeat
    /// counts once, and the first fault names its line without printing the token. It
    /// catches a file whose second token is silently ignored (rotation that never takes
    /// effect), a Windows-edited file that fails every agent, and a refusal that leaks
    /// the secret into a log.
    #[test]
    fn a_file_holds_one_token_per_line() {
        let tokens = parse(format!("{A}\r\n\n  \n{B}\n{A}\n").as_bytes()).unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(parse(b"\n \n").unwrap_err(), "holds no token");
        let short = parse(format!("{A}\nshort\n").as_bytes()).unwrap_err();
        assert!(short.starts_with("line 2: "), "{short}");
        assert!(!short.contains("short"), "{short}");
        let spaced = parse(format!("{A}\nhas space inside the token line\n").as_bytes());
        assert!(spaced.unwrap_err().starts_with("line 2: "));
        let many = (0..=MAX_AGENT_TOKENS)
            .map(|n| format!("{A}-{n}\n"))
            .collect::<String>();
        let reason = parse(many.as_bytes()).unwrap_err();
        assert!(reason.contains("more than 16"), "{reason}");
    }
}
