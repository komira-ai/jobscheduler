//! The Tailscale OAuth client: an id and a secret, read from a file that only its owner
//! can use, the posture of every secret file of the service.

use std::path::Path;

use js_elastic::Secret;
use serde::Deserialize;

use super::NetworkError;
use crate::config::read_private;

/// The most bytes an OAuth client file may hold; it is two short lines.
pub const OAUTH_FILE_LIMIT: usize = 4096;

/// The name the start-up check reports a refusal under.
const FLAG: &str = "network.oauth_client_file";

/// What the file holds. It has no `Debug`, so its secret cannot be printed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContent {
    client_id: String,
    client_secret: String,
}

/// An OAuth client of a tailnet. It is held so that its secret cannot be printed: the
/// `Debug` shows the id and `***`.
pub struct OAuthClient {
    id: String,
    secret: Secret,
}

impl OAuthClient {
    /// A client of `id` and `secret`.
    pub fn new(id: impl Into<String>, secret: Secret) -> Self {
        Self {
            id: id.into(),
            secret,
        }
    }

    /// Reads the client from the TOML file at `path` (`client_id` and `client_secret`).
    /// The file must be a regular file owned by the user running the service, mode 0600
    /// or 0400, of at most [`OAUTH_FILE_LIMIT`] bytes.
    ///
    /// # Errors
    ///
    /// [`NetworkError::ClientFile`] saying which rule the file broke. The message holds
    /// nothing the file says.
    pub fn from_file(path: &Path) -> Result<Self, NetworkError> {
        let bytes = read_private(path, FLAG)
            .map_err(|err| NetworkError::ClientFile(err.reason().to_owned()))?;
        let refuse = |why: &str| NetworkError::ClientFile(format!("{}: {why}", path.display()));
        if bytes.len() > OAUTH_FILE_LIMIT {
            return Err(refuse("is longer than the limit"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| refuse("is not UTF-8 text"))?;
        // Neither `message` nor `Display`: both can quote the value the parser stopped at
        // (a type error repeats it), and the value may be the secret. Only the line is said.
        let content: FileContent = toml::from_str(text).map_err(|err| {
            // The line the parser stopped at; the first when it gives no position.
            let start = err.span().map_or(0, |span| span.start.min(text.len()));
            let line = 1 + text.as_bytes()[..start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count();
            refuse(&format!("is not a valid client file (line {line})"))
        })?;
        if content.client_id.is_empty() || content.client_secret.is_empty() {
            return Err(refuse("client_id and client_secret must not be empty"));
        }
        Ok(Self::new(
            content.client_id,
            Secret::new(content.client_secret),
        ))
    }

    /// The `application/x-www-form-urlencoded` body of the client-credentials grant.
    pub(crate) fn grant_body(&self) -> Secret {
        Secret::new(format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            form_encode(&self.id),
            form_encode(self.secret.expose())
        ))
    }
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("OAuthClient")
            .field("id", &self.id)
            .field("secret", &self.secret)
            .finish()
    }
}

/// `text` percent-encoded for a form: unreserved characters stay, every other byte is
/// `%XX`.
fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a form value keeps unreserved characters and encodes the rest by
    /// byte, so a secret with `&`, `=`, `+`, a space or a multi-byte character cannot
    /// add a field or be misread.
    #[test]
    fn a_form_value_is_percent_encoded() {
        assert_eq!(form_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(form_encode("a&b=c+d e"), "a%26b%3Dc%2Bd%20e");
        assert_eq!(form_encode("é"), "%C3%A9");
        assert_eq!(form_encode(""), "");
    }

    /// Asserts that the grant body carries the three fields, and that the client prints
    /// its id and `***`.
    #[test]
    fn the_grant_body_names_the_client_and_debug_hides_the_secret() {
        let client = OAuthClient::new("id-1", Secret::new("sh/ss".to_owned()));
        assert_eq!(
            client.grant_body().expose(),
            "grant_type=client_credentials&client_id=id-1&client_secret=sh%2Fss"
        );
        let shown = format!("{client:?}");
        assert!(shown.contains("id-1"), "{shown}");
        assert!(shown.contains("***"), "{shown}");
        assert!(!shown.contains("sh/ss"), "{shown}");
    }
}
