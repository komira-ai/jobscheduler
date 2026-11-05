//! The transport port: one HTTP exchange, bodies as bytes. [`crate::UreqTransport`] is the
//! production one; the fake store of the `fake` feature is another.
//!
//! A request's `Debug` shows its method, its URL without the query and its header names, never a
//! header's value (the `authorization` and `x-amz-security-token` values are secrets) or the
//! body.

use std::fmt;
use std::io;

/// An HTTP method the client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `POST`.
    Post,
}

impl Method {
    /// The method's name, upper case.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Post => "POST",
        }
    }
}

/// One request: the method, the full URL, the headers to send (lower-case names) and the body.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The URL, with its query.
    pub url: String,
    /// The headers, by lower-case name.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The value of the header `name` (lower case), if the request has one.
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        let url = self
            .url
            .split_once('?')
            .map_or(self.url.as_str(), |(path, _)| path);
        out.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &url)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// One answer: the status, the headers (lower-case names) and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The headers, by lower-case name.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The value of the header `name` (lower case), if the answer has one.
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

/// Why an exchange did not produce an answer. It names the kind of failure only: no URL, header
/// or body is in it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// No answer came within the time limit.
    #[error("the store did not answer in time")]
    Timeout,
    /// The connection could not be made or broke.
    #[error("the connection failed ({0:?})")]
    Io(io::ErrorKind),
    /// The answer's body is longer than the transport reads.
    #[error("the answer is longer than the transport reads")]
    TooLarge,
    /// The request could not be built (a method, URL or header the HTTP library refuses).
    #[error("the request cannot be sent as given")]
    Request,
    /// The exchange failed in another way (an answer that is not HTTP, for one).
    #[error("the exchange failed")]
    Exchange,
}

/// One HTTP exchange. An implementation sends exactly one request, follows no redirect, and
/// does not retry.
pub trait Transport: Send + Sync {
    /// Sends `request` and returns the answer, whatever its status.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no answer came.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The value of the header `name` in `headers`.
fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(have, _)| have == name)
        .map(|(_, value)| value.as_str())
}
