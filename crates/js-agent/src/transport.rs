//! How the agent reaches the server: one POST of a JSON body, one answer.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use js_client::{Client, ClientError};
use serde_json::Value;

/// The longest the agent waits for one answer. A heartbeat answered later than this is
/// close to the server's grace period anyway.
pub const REQUEST_LIMIT: Duration = Duration::from_secs(10);

/// What the server answered. Its `Debug` text leaves the body out: the body of a welcome
/// holds the session secret, and a `{:?}` in a log line or a panic message must not print
/// it.
#[derive(Clone, PartialEq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// The body as JSON, `null` when it was empty or not JSON.
    pub body: Value,
    /// The wait a `Retry-After` header asked for, in whole seconds.
    pub retry_after: Option<Duration>,
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Answer")
            .field("status", &self.status)
            .field("body", &format_args!("<redacted>"))
            .field("retry_after", &self.retry_after)
            .finish()
    }
}

/// A carrier of the agent's two messages. The first implementation is HTTP/1.1 through
/// `js-client`; a test supplies a fake.
pub trait Transport {
    /// Sends `body` to `path` and returns the answer, whatever its status. The error is a
    /// short reason the server could not be reached; it never holds the token.
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String>;

    /// Makes `secret` the bearer of every later post, in place of the token the transport
    /// was connected with. The agent calls it with the host credential a welcome carried
    /// (once it is on disk) and with the one it finds in its state root at start.
    ///
    /// # Errors
    ///
    /// A short reason the secret cannot be used as a bearer; it never holds the secret.
    /// The transport then keeps what it had.
    fn adopt_credential(&mut self, secret: &str) -> Result<(), String>;
}

/// Makes the [`Transport`] the flags name.
pub trait Connector {
    /// Checks `--server`, `--token-file` and `--ca-cert` and connects.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming the flag that is wrong.
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError>;
}

/// The server over HTTP/1.1, through the pinned-root client of `js-client`.
#[derive(Debug, Clone, Copy)]
pub struct Dialer;

impl Connector for Dialer {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        Client::connect(server, token_file, ca_cert, REQUEST_LIMIT)
            .map(|client| Box::new(HttpTransport(client)) as Box<dyn Transport>)
    }
}

/// A connected client. It holds the token, or the credential that replaced it, and has no
/// `Debug`.
struct HttpTransport(Client);

impl Transport for HttpTransport {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        self.0
            .post(path, body)
            .map(|reply| Answer {
                status: reply.status,
                body: reply.body,
                retry_after: reply.retry_after,
            })
            .map_err(|err| err.to_string())
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.0.set_bearer(secret).map_err(|err| err.to_string())
    }
}
