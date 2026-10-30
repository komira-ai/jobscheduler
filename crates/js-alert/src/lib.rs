//! Where an alert goes: a sink an operator reads, and the rule that says each alert once.
//!
//! - [`AlertSink`] takes one [`Alert`] (a key and a text) and delivers it. [`WebhookSink`]
//!   POSTs a small JSON body to a URL the configuration file names; [`FakeSink`] keeps what
//!   it was given, for tests. There is no mail sink: the workspace holds no SMTP client,
//!   and a mail implementation is one more `impl AlertSink` when one is added.
//! - [`Said`] is the dedupe. Each pass hands it the alerts that hold now; it sends the ones
//!   it has not said, stays quiet about the ones it has, and forgets an alert once it no
//!   longer holds, so one that clears and returns is said again. An alert the sink refused
//!   is sent again at the next pass.
//! - [`http`] holds the URL rule and the HTTP agent the webhook and the watcher share.
//!
//! Nothing here reads the environment: the recipient is a value the caller read from its
//! configuration file.

pub mod http;
mod receiver;
mod said;
mod sink;
mod webhook;

pub use receiver::{Receiver, Seen};
pub use said::Said;
pub use sink::{Alert, AlertSink, FakeSink, SinkError};
pub use webhook::{DELIVERY_LIMIT, WebhookSink};
