//! Where an alert goes: a sink an operator reads, and the rule that says each alert once.
//!
//! - [`AlertSink`] takes one [`Alert`] (a key and a text) and delivers it. [`WebhookSink`]
//!   POSTs a small JSON body to a URL the configuration file names; [`FakeSink`] keeps what
//!   it was given, for tests. There is no mail sink: the workspace holds no SMTP client,
//!   and a mail implementation is one more `impl AlertSink` when one is added.
//! - [`Said`] is the dedupe, with time as an input. Each pass hands it the alerts that hold;
//!   it returns the ones to send: a new one, a holding one every `repeat_after` (so one lost
//!   message does not leave a leak announced only once), and one the sink refused after a
//!   wait that doubles up to a cap. A refused alert stays pending until it has been
//!   delivered once, even if it clears meanwhile; one that clears after delivery and
//!   returns is said again.
//! - [`Notifier`] is [`Said`] plus a delivery thread and a bounded queue: a pass never
//!   waits on the sink, so a receiver that refuses or hangs cannot delay its caller. A full
//!   queue drops the oldest alert (counted) and the alert is sent again later.
//! - [`http`] holds the URL rule and the HTTP agent the webhook and the watcher share.
//!
//! Nothing here reads the environment: the recipient is a value the caller read from its
//! configuration file.

pub mod http;
mod notifier;
mod receiver;
mod said;
mod sink;
mod webhook;

pub use notifier::Notifier;
pub use receiver::{Receiver, Seen};
pub use said::{RETRY_BASE, RETRY_CAP, Said};
pub use sink::{Alert, AlertSink, FakeSink, SinkError};
pub use webhook::{DELIVERY_LIMIT, WebhookSink};
