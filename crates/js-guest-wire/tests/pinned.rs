//! The bytes of the guest channel, pinned from outside the crate.
//!
//! A guest image that is already built speaks these bytes, so none may change without a
//! new protocol version. The unit tests of `wire.rs` pin each message layout; these pin
//! what a peer sees on the stream: the constants, the length prefix, the kind byte of
//! every message, and the probe frames' JSON.

use js_guest_wire::frame::{self, Frame, Kind, MAX_BODY_BYTES, MAX_FRAME_BYTES};
use js_guest_wire::wire::{
    End, Exit, GuestMsg, HostMsg, MAX_FRAME, OutputEntry, OutputKind, Refusal, RunRequest,
    TOKEN_LEN, Usage, VERSION, read_frame, write_frame,
};

/// Catches a constant of the run protocol changed in place: a guest of another version
/// refuses the `Hello`, and a changed limit or token length changes what every peer
/// accepts.
#[test]
fn the_constants_are_the_ones_guest_images_speak() {
    assert_eq!(VERSION, 1);
    assert_eq!(MAX_FRAME, 1 << 20);
    assert_eq!(TOKEN_LEN, 32);
    assert_eq!(MAX_FRAME_BYTES, 4 << 20);
    assert_eq!(MAX_BODY_BYTES, 1 << 20);
}

/// Catches a stream framing that drifts from a 4-byte big-endian length before the kind
/// and body: the first bytes a guest reads of a `Hello` are the length 35, kind `01`
/// and the version, and nothing follows the token.
#[test]
fn a_hello_on_the_stream_is_length_kind_version_token() {
    let hello = HostMsg::Hello {
        version: VERSION,
        token: [0x5a; TOKEN_LEN],
    };
    let mut stream = Vec::new();
    write_frame(&mut stream, &hello.encode()).expect("write");
    let mut want = vec![0, 0, 0, 35, 0x01, 0x00, 0x01];
    want.extend([0x5a; TOKEN_LEN]);
    assert_eq!(stream, want);
    let frame = read_frame(&mut stream.as_slice()).expect("read");
    assert_eq!(HostMsg::decode(&frame).expect("decode"), hello);
}

/// Catches a message kind renumbered: the host's kinds are `01`-`7f` and the guest's
/// `81`-`ff`, and a peer tells them apart by this byte.
#[test]
fn every_message_starts_with_its_kind_byte() {
    let kinds = [
        (
            HostMsg::Hello {
                version: 9,
                token: [0; TOKEN_LEN],
            }
            .encode(),
            0x01,
        ),
        (HostMsg::Run(RunRequest::default()).encode(), 0x02),
        (HostMsg::Kill.encode(), 0x03),
        (
            GuestMsg::Ready {
                version: 1,
                session: String::new(),
            }
            .encode(),
            0x81,
        ),
        (
            GuestMsg::Refused {
                reason: Refusal::Version,
                detail: String::new(),
            }
            .encode(),
            0x82,
        ),
        (GuestMsg::Started { pid: 1 }.encode(), 0x83),
        (
            GuestMsg::Exited(Exit {
                end: End::Killed,
                usage: Usage::default(),
                outputs: Vec::new(),
                stragglers: false,
            })
            .encode(),
            0x84,
        ),
    ];
    for (bytes, kind) in kinds {
        assert_eq!(bytes[0], kind, "{bytes:02x?}");
    }
}

/// Catches a reason or output kind renumbered: they travel as one byte each, and a host
/// reads `Refused` by it.
#[test]
fn reasons_and_output_kinds_are_one_byte_values() {
    let reasons = [
        (Refusal::Version, 1),
        (Refusal::Token, 2),
        (Refusal::AlreadyRan, 3),
        (Refusal::BadRequest, 4),
        (Refusal::NoHello, 5),
        (Refusal::StartFailed, 6),
    ];
    for (reason, byte) in reasons {
        let msg = GuestMsg::Refused {
            reason,
            detail: String::new(),
        };
        assert_eq!(msg.encode(), [0x82, byte, 0, 0, 0, 0], "{reason:?}");
    }
    let kinds = [
        (OutputKind::Missing, 0),
        (OutputKind::File, 1),
        (OutputKind::Directory, 2),
        (OutputKind::Symlink, 3),
        (OutputKind::Other, 4),
    ];
    for (kind, byte) in kinds {
        let exit = Exit {
            end: End::TimedOut,
            usage: Usage::default(),
            outputs: vec![OutputEntry {
                path: String::new(),
                kind,
                size: 0,
            }],
            stragglers: false,
        };
        let bytes = GuestMsg::Exited(exit).encode();
        // kind, end tag and value, four usage words, the count, an empty path, then the
        // output kind.
        assert_eq!(bytes[1 + 1 + 4 + 32 + 4 + 4], byte, "{kind:?}");
    }
}

/// Catches a probe frame whose JSON drifts (key names, key order, a kind spelled
/// differently), which the host's listener and the guest's probes both depend on.
#[test]
fn probe_frames_are_a_length_then_json() {
    let kinds = [
        (Kind::Ready, "ready"),
        (Kind::Result, "result"),
        (Kind::Log, "log"),
        (Kind::Shutdown, "shutdown"),
        (Kind::Ack, "ack"),
        (Kind::Refused, "refused"),
    ];
    for (kind, spelled) in kinds {
        let bytes = frame::encode(&Frame::new(kind, "n", "b")).expect("encode");
        let json = format!(r#"{{"kind":"{spelled}","name":"n","body":"b"}}"#);
        let mut want = u32::try_from(json.len())
            .expect("small")
            .to_be_bytes()
            .to_vec();
        want.extend(json.as_bytes());
        assert_eq!(bytes, want, "{spelled}");
    }
}
