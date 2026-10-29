//! The real client against servers started in the test process on 127.0.0.1, over plain
//! http and over TLS with a certificate made for the test. Their answers are fixtures
//! written from the documented shapes.

mod errors;
mod fixtures;
mod inputs;
mod launch;
mod proxy;
mod reads;
mod setup;
mod support;
mod tls;
mod transport;
