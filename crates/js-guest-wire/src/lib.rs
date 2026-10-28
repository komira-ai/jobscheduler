//! The bytes between a macOS VM guest's agent and its host: the wire of `js-guest`.
//!
//! - [`wire`]: the framed, versioned messages of the run protocol (`Hello`, `Run`, `Kill`
//!   and the guest's answers), byte for byte, with the golden bytes that pin each layout.
//! - [`frame`]: the JSON frames the probe guests send over a virtio socket, and the
//!   host's replies.
//!
//! This crate holds the codecs and nothing else: it reads and writes any byte stream,
//! opens no socket and reads no clock, so the guest agent, the host side and the tests
//! share one codec. The bytes are a compatibility surface: a guest image already built
//! speaks them, so a change here is a new protocol version, never an edit.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod frame;
pub mod wire;
