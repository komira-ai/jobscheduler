//! The fake minter: its script, its counts and its record.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use js_logmint::fake::FakeMinter;
use js_logmint::{MintError, Minter};
use js_logstore::{Metadata, StoreError, TransportError, Unavailable};

use crate::support::{Rig, request};

/// Asserts the script: `fail_next(2, ..)` answers the next two mints with that error and the
/// third mints; `block` holds a later mint after it is counted and recorded, without issuing,
/// until `release`, and the test learns that it is held from the minter, not from time;
/// calls are counted per host, failed and held ones included, and every request is recorded in
/// order; and `expire_all` makes every credential the minter issued answer `ExpiredToken` in
/// the store. It catches a script that does not take effect, a count shared between hosts and
/// a block that issues anyway.
#[test]
fn a_fake_minter_can_be_scripted() {
    let rig = Rig::new();
    let minter = Arc::new(FakeMinter::new(rig.fake.clone()));
    assert!(minter.store().objects().is_empty());
    let down = MintError::Unavailable(Unavailable::Transport(TransportError::Timeout));
    minter.fail_next(2, down.clone());
    let first = request("a/b", "host-1");
    assert_eq!(minter.mint(&first), Err(down.clone()));
    assert_eq!(minter.mint(&first), Err(down));
    let credential = minter.mint(&first).unwrap();
    assert_eq!(minter.calls("host-1"), 3);
    assert_eq!(minter.calls("host-2"), 0);

    minter.block();
    let (done, result) = mpsc::channel();
    let held = Arc::clone(&minter);
    let second = request("a/b", "host-2");
    let asked = second.clone();
    let mint = thread::spawn(move || done.send(held.mint(&asked)).unwrap());
    minter.wait_for_held(1);
    assert_eq!(minter.calls("host-2"), 1);
    assert_eq!(
        minter.requests(),
        [first.clone(), first.clone(), first, second]
    );
    assert!(result.try_recv().is_err());
    minter.release();
    mint.join().unwrap();
    let late = result.recv().unwrap().unwrap();
    assert_eq!(minter.calls("host-1"), 3);

    let none = Metadata::new();
    let key = "a/b/job-1/stdout.log";
    let clients = [rig.client(credential), rig.client(late)];
    for client in &clients {
        client.put(key, b"x".to_vec(), &none).unwrap();
    }
    minter.expire_all();
    for client in &clients {
        assert_eq!(
            client.put(key, b"x".to_vec(), &none),
            Err(StoreError::Expired {
                code: "ExpiredToken".to_owned()
            })
        );
    }
}
