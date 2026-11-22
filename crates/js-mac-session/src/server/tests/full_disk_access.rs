//! The `full-disk-access` verb over the socket: the real helper's answers, and the
//! client's reading of a reply it does not expect.

use std::io;

use super::*;
use crate::proto::FullDiskAccess;

/// Asserts that the client's `full_disk_access` gets each state the host shows over the
/// socket (granted, then not granted, then undetermined), on a helper whose boot reset
/// has not run. Catches: the verb routed to another reply, or refused before the reset.
#[test]
fn the_verb_works_over_the_socket() {
    let served = served_from("fda", Scripted::fixed(Ok(())), false);
    for (kind, want) in [
        (None, FullDiskAccess::Granted),
        (
            Some(io::ErrorKind::PermissionDenied),
            FullDiskAccess::NotGranted,
        ),
        (Some(io::ErrorKind::NotFound), FullDiskAccess::Undetermined),
    ] {
        served.host.state().privacy_dir = kind;
        assert_eq!(served.client.full_disk_access().unwrap(), want);
    }
}

/// Asserts that a helper's refusal as an unknown verb, which is what a helper from
/// before the verb answers, reaches the caller as an error that reads as
/// `unknown_to_helper`, and that a reply of another kind is the client's own
/// `Unexpected` error naming it. Catches: the old helper's refusal turned into a
/// state (the agent would take it for an answer), or a wrong reply taken as one.
#[test]
fn an_old_helper_and_a_wrong_reply_are_errors() {
    let dir = scratch("server-fda-client");
    let path = dir.join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let answers = [
        Reply::Refused {
            reason: "bad request: unknown variant `full-disk-access`, expected one of `x`"
                .to_owned(),
        },
        Reply::Killed,
    ];
    std::thread::spawn(move || {
        for answer in answers {
            let (stream, _) = listener.accept().unwrap();
            let hello = Reply::Hello {
                nonce: "n".to_owned(),
            };
            proto::send(stream.as_fd(), &hello, &[]).unwrap();
            let _ = proto::recv::<Call>(stream.as_fd(), RUN_FDS);
            proto::send(stream.as_fd(), &answer, &[]).unwrap();
        }
    });
    let client = Client::new(&path);
    match client.full_disk_access() {
        Err(error @ ClientError::Refused(_)) => assert!(error.unknown_to_helper()),
        other => panic!("{other:?}"),
    }
    match client.full_disk_access() {
        Err(error @ ClientError::Unexpected(_)) => assert!(error.to_string().contains("Killed")),
        other => panic!("{other:?}"),
    }
}
