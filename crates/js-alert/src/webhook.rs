//! The webhook sink: one HTTP POST of a small JSON body per alert.

use std::fmt;
use std::time::Duration;

use serde_json::json;
use ureq::Agent;

use crate::http::{Plain, UrlError, agent, check};
use crate::sink::{Alert, AlertSink, SinkError};

/// How long one delivery may take, connect and TLS included.
pub const DELIVERY_LIMIT: Duration = Duration::from_secs(10);

/// Delivers an alert as `POST url` with the body
/// `{"source": ..., "key": ..., "text": ...}` (`content-type: application/json`) and an
/// optional `authorization` header. A status outside 200 to 299 is a failure. The URL is
/// `https://`, or `http://` to a loopback address; it and the header are never printed or
/// put in an error, because a webhook URL is often the secret itself.
pub struct WebhookSink {
    agent: Agent,
    url: String,
    authorization: Option<String>,
    source: String,
}

impl fmt::Debug for WebhookSink {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("WebhookSink")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl WebhookSink {
    /// A sink for `url` that names itself `source` in each body (so one receiver can tell
    /// the lease server from the watcher), sending `authorization` as the header of that
    /// name when given (`Bearer <token>`, read by the caller from a file). `limit` bounds
    /// one delivery; [`DELIVERY_LIMIT`] is a fair value.
    ///
    /// # Errors
    ///
    /// [`UrlError`] when the URL is not one the sink may dial.
    pub fn new(
        url: &str,
        authorization: Option<String>,
        source: &str,
        limit: Duration,
    ) -> Result<Self, UrlError> {
        let https = check(url, Plain::LoopbackOnly)?;
        Ok(Self {
            agent: agent(https, limit),
            url: url.to_owned(),
            authorization,
            source: source.to_owned(),
        })
    }
}

impl AlertSink for WebhookSink {
    fn send(&self, alert: &Alert) -> Result<(), SinkError> {
        let body = json!({ "source": self.source, "key": alert.key, "text": alert.text });
        let mut request = self
            .agent
            .post(self.url.as_str())
            .header("content-type", "application/json");
        if let Some(authorization) = &self.authorization {
            request = request.header("authorization", authorization);
        }
        let response = request
            .send(body.to_string().as_bytes())
            .map_err(|_| SinkError::new("the webhook could not be reached"))?;
        let status = response.status().as_u16();
        match (200..300).contains(&status) {
            true => Ok(()),
            false => Err(SinkError::new(format!("the webhook answered {status}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::Receiver;

    fn alert() -> Alert {
        Alert {
            key: "orphan:gpu:i-1".to_owned(),
            text: "a machine of ours no record knows".to_owned(),
        }
    }

    fn sink(url: &str, token: Option<&str>) -> WebhookSink {
        let authorization = token.map(|token| format!("Bearer {token}"));
        WebhookSink::new(url, authorization, "lease-server", DELIVERY_LIMIT).unwrap()
    }

    /// Asserts what goes on the wire: one POST whose JSON body carries the source, the key
    /// and the text, with the content type and the authorization header the sink was given.
    /// It catches a renamed field (a receiver's parser would silently see nothing) and a
    /// token that is not sent.
    #[test]
    fn an_alert_is_one_post_with_a_json_body_and_the_token() {
        let receiver = Receiver::start(200);
        let url = format!("http://{}/hook", receiver.addr());
        sink(&url, Some("tok-1")).send(&alert()).unwrap();
        let seen = receiver.requests();
        assert_eq!(seen.len(), 1);
        let head = seen[0].head.to_ascii_lowercase();
        assert!(head.starts_with("post /hook http/1.1"), "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");
        assert!(head.contains("authorization: bearer tok-1"), "{head}");
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            json!({
                "source": "lease-server",
                "key": "orphan:gpu:i-1",
                "text": "a machine of ours no record knows"
            })
        );
    }

    /// Asserts that a sink with no token sends no authorization header. It catches a
    /// header built from an empty token.
    #[test]
    fn without_a_token_there_is_no_authorization_header() {
        let receiver = Receiver::start(204);
        let url = format!("http://{}/", receiver.addr());
        sink(&url, None).send(&alert()).unwrap();
        let head = receiver.requests()[0].head.to_ascii_lowercase();
        assert!(!head.contains("authorization"), "{head}");
    }

    /// Asserts that a non-2xx answer and an unreachable receiver are both failures, and
    /// that neither error repeats the URL or the token. It catches an alert counted as
    /// delivered because the HTTP exchange completed, and a secret leaking into a log
    /// line through the error.
    #[test]
    fn a_refusal_and_an_unreachable_receiver_are_failures_that_name_no_secret() {
        let receiver = Receiver::start(500);
        let url = format!("http://{}/SECRET-path", receiver.addr());
        let refused = sink(&url, Some("SECRET-token")).send(&alert()).unwrap_err();
        assert_eq!(refused.to_string(), "the webhook answered 500");

        // A held, non-listening port: a dropped listener would free the number for any
        // other test to bind.
        let url = format!("http://{}/SECRET-path", js_testkit::port::dead_addr());
        let down = sink(&url, Some("SECRET-token")).send(&alert()).unwrap_err();
        assert_eq!(down.to_string(), "the webhook could not be reached");
        let shown = format!("{:?}", sink(&url, Some("SECRET-token")));
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    /// Asserts that a URL the rule refuses makes no sink, and an https URL makes one
    /// without dialing it. It catches a constructor that skips the URL check.
    #[test]
    fn the_constructor_applies_the_url_rule() {
        let refused = WebhookSink::new("http://10.0.0.5/hook", None, "x", DELIVERY_LIMIT);
        assert_eq!(refused.unwrap_err(), UrlError::PlainHttp);
        let made = WebhookSink::new("https://hooks.example.org/x", None, "x", DELIVERY_LIMIT);
        assert!(made.is_ok());
    }
}
