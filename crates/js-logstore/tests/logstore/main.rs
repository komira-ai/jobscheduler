//! The tests of js-logstore, in one binary so that every line and branch is covered by the
//! tests of one profile: the layout, the signer against the published vectors, the client
//! against the fake store and against scripted answers, and `UreqTransport` against a server on
//! 127.0.0.1. No test sleeps or reads the time: the fake store and the clients share a
//! `ManualClock`. Only the transport tests open sockets, all on 127.0.0.1 with port 0.

mod answers;
mod endpoint;
mod fake_requests;
mod fake_sts;
mod layout;
mod secrets;
mod server;
mod signer;
mod store;
mod support;
mod transport;
mod vectors;
