//! The client against the fake store: scoping, signatures, metadata, ranges, listing, expiry,
//! scripted failures and the arguments refused before sending.

use std::sync::{Arc, Mutex};
use std::thread;

use js_logstore::fake::{Action, FakeStore, ISSUED_TTL, Op, Status};
use js_logstore::layout::{FIRST_BYTE_AT_MS, JobPrefix, MAX_PUT_BYTES, Stream};
use js_logstore::{
    HttpRequest, HttpResponse, MAX_GET_BYTES, ManualClock, Metadata, Store, StoreError, Transport,
    TransportError, Unavailable,
};

use crate::support::{ALL, Rig, START, at, policy};

/// A transport that changes each request with `change` before the fake store sees it, and
/// keeps the requests as the fake saw them.
struct Tamper<F> {
    fake: FakeStore,
    change: F,
    seen: Seen,
}

impl<F: Fn(&mut HttpRequest) + Send + Sync> Transport for Tamper<F> {
    fn send(&self, mut request: HttpRequest) -> Result<HttpResponse, TransportError> {
        (self.change)(&mut request);
        self.seen.lock().unwrap().push(request.clone());
        self.fake.send(request)
    }
}

/// The requests a tampering transport passed on.
type Seen = Arc<Mutex<Vec<HttpRequest>>>;

/// A client over a tampering transport.
type Tampered<F> = Store<Tamper<F>, Arc<ManualClock>>;

/// A client of `rig`'s fake whose requests pass through `change`, with a credential that may
/// do everything, and the record of the requests the fake saw.
fn tampered<F: Fn(&mut HttpRequest) + Send + Sync>(rig: &Rig, change: F) -> (Tampered<F>, Seen) {
    let seen = Arc::new(Mutex::default());
    let transport = Tamper {
        fake: rig.fake.clone(),
        change,
        seen: Arc::clone(&seen),
    };
    let credential = rig.fake.issue(policy(&ALL, ""));
    let store = Store::new(
        transport,
        rig.clock.clone(),
        rig.endpoint.clone(),
        credential,
    );
    (store, seen)
}

/// The refusal of a signature the fake did not compute.
fn mismatch() -> StoreError {
    StoreError::Denied {
        status: 403,
        code: "SignatureDoesNotMatch".to_owned(),
    }
}

/// The refusal of a policy.
fn denied() -> StoreError {
    StoreError::Denied {
        status: 403,
        code: "AccessDenied".to_owned(),
    }
}

/// Metadata of one first-byte time.
fn first_byte_at(ms: &str) -> Metadata {
    Metadata::from([(FIRST_BYTE_AT_MS.to_owned(), ms.to_owned())])
}

/// Asserts that a credential issued for put and delete under one job's prefix puts and deletes
/// there, and that a put or delete under another job's prefix (including one whose id only
/// begins like this one's), a get and a list are all refused as `Denied`, with the fake's
/// objects and accepted operations unchanged; and that a read credential lists and gets but
/// cannot put. Catches a fake that does not enforce the policy, which would make every
/// dependent's scoping test worthless.
#[test]
fn a_credential_for_one_job_cannot_touch_another() {
    let rig = Rig::new();
    let p1 = JobPrefix::new("js", "job-1").unwrap();
    let p2 = JobPrefix::new("js", "job-2").unwrap();
    let p10 = JobPrefix::new("js", "job-10").unwrap();
    let admin = rig.admin();
    admin
        .put(
            &p2.chunk_key(Stream::Stdout, 0),
            b"theirs".to_vec(),
            &Metadata::new(),
        )
        .unwrap();

    let writer = rig.client(
        rig.fake
            .issue(policy(&[Action::Put, Action::Delete], p1.as_str())),
    );
    let mine = p1.chunk_key(Stream::Stdout, 0);
    writer
        .put(&mine, b"mine".to_vec(), &first_byte_at("1"))
        .unwrap();
    writer
        .put(
            &p1.final_key(Stream::Stdout),
            b"mine".to_vec(),
            &Metadata::new(),
        )
        .unwrap();
    writer.delete(&mine).unwrap();

    let objects = rig.fake.objects();
    let operations = rig.fake.operations();
    let theirs = p2.chunk_key(Stream::Stdout, 0);
    assert_eq!(
        writer.put(&theirs, b"evil".to_vec(), &Metadata::new()),
        Err(denied())
    );
    assert_eq!(writer.delete(&theirs), Err(denied()));
    let neighbour = p10.chunk_key(Stream::Stdout, 0);
    assert_eq!(
        writer.put(&neighbour, b"evil".to_vec(), &Metadata::new()),
        Err(denied())
    );
    assert_eq!(
        writer.get_range(&p1.final_key(Stream::Stdout), 0, 4),
        Err(denied())
    );
    assert_eq!(writer.list(p1.as_str(), None, 10), Err(denied()));
    assert_eq!(rig.fake.objects(), objects);
    assert_eq!(rig.fake.operations(), operations);

    let reader = rig.client(rig.fake.issue(policy(&[Action::Get, Action::List], "js/")));
    assert_eq!(reader.list("js/", None, 10).unwrap().entries.len(), 2);
    let object = reader.get_range(&theirs, 0, 6).unwrap().unwrap();
    assert_eq!(object.bytes, b"theirs");
    assert_eq!(reader.list("", None, 10), Err(denied()));
    assert_eq!(
        reader.put(&theirs, b"x".to_vec(), &Metadata::new()),
        Err(denied())
    );
    assert_eq!(reader.delete(&theirs), Err(denied()));
    assert_eq!(rig.fake.objects(), objects);
}

/// Asserts that a request signed with a wrong secret, one whose body, key, metadata or range
/// was changed after signing, one whose session token was removed, one with no signature at
/// all, and one from a key the fake never issued are each refused with 403 and store nothing;
/// the client reads the last as an expired credential (`InvalidAccessKeyId`: the store does not
/// know the key, as it forgets a temporary key past its expiry). Catches a fake that accepts an
/// unsigned or altered request.
#[test]
fn a_bad_signature_is_refused_and_changes_nothing() {
    let rig = Rig::new();
    let key = "js/job-1/stdout/00000000000000000000";
    let admin = rig.admin();
    admin
        .put(key, b"0123456789".to_vec(), &Metadata::new())
        .unwrap();
    let objects = rig.fake.objects();
    let operations = rig.fake.operations();

    let mut wrong = rig.fake.issue(policy(&ALL, ""));
    wrong.secret_access_key.push('x');
    let wrong_secret = rig.client(wrong);
    assert_eq!(
        wrong_secret.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );
    assert_eq!(wrong_secret.delete(key), Err(mismatch()));

    let (body, _) = tampered(&rig, |request| request.body.push(b'!'));
    assert_eq!(
        body.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );

    let (moved, _) = tampered(&rig, |request| {
        request.url = request
            .url
            .replace("/00000000000000000000", "/00000000000000000001")
    });
    assert_eq!(
        moved.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );
    assert_eq!(moved.delete(key), Err(mismatch()));

    let (metadata, _) = tampered(&rig, |request| {
        request
            .headers
            .push(("x-amz-meta-extra".to_owned(), "1".to_owned()))
    });
    assert_eq!(
        metadata.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );

    let (range, _) = tampered(&rig, |request| {
        for (name, value) in &mut request.headers {
            if name == "range" {
                *value = "bytes=0-1".to_owned();
            }
        }
    });
    assert_eq!(range.get_range(key, 0, 5), Err(mismatch()));

    let (tokenless, _) = tampered(&rig, |request| {
        request
            .headers
            .retain(|(name, _)| name != "x-amz-security-token")
    });
    assert_eq!(
        tokenless.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );

    let (unsigned, _) = tampered(&rig, |request| {
        request.headers.retain(|(name, _)| name != "authorization")
    });
    assert_eq!(
        unsigned.put(key, b"other".to_vec(), &Metadata::new()),
        Err(mismatch())
    );

    let mut stranger = rig.fake.issue(policy(&ALL, ""));
    stranger.access_key_id = "AKIDNEVERISSUED".to_owned();
    assert_eq!(
        rig.client(stranger)
            .put(key, b"other".to_vec(), &Metadata::new()),
        Err(StoreError::Expired {
            code: "InvalidAccessKeyId".to_owned()
        })
    );

    assert_eq!(rig.fake.objects(), objects);
    assert_eq!(rig.fake.operations(), operations);
}

/// Asserts that a chunk's `x-amz-meta-first-byte-at-ms` is sent signed with the content type
/// `application/octet-stream`, stored, and returned by `get_range`; that a ranged get returns
/// exactly the bytes asked, or those up to the end, with the total length; that a read from the
/// end is `OutOfRange`; and that a missing key is `None`.
#[test]
fn put_carries_the_metadata_and_get_returns_it() {
    let rig = Rig::new();
    let (store, seen) = tampered(&rig, |_| {});
    let key = "js/job-1/stdout/00000000000000000000";
    let bytes: Vec<u8> = (0..100).collect();
    let metadata = first_byte_at("1800000000250");
    store.put(key, bytes.clone(), &metadata).unwrap();

    let put = seen.lock().unwrap().remove(0);
    assert_eq!(
        put.header("x-amz-meta-first-byte-at-ms"),
        Some("1800000000250")
    );
    assert_eq!(put.header("content-type"), Some("application/octet-stream"));
    let authorization = put.header("authorization").unwrap();
    assert!(authorization.contains(
        "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-meta-first-byte-at-ms;x-amz-security-token,"
    ));

    let object = store.get_range(key, 10, 20).unwrap().unwrap();
    assert_eq!(object.bytes, bytes[10..30]);
    assert_eq!(object.total_len, 100);
    assert_eq!(object.metadata, metadata);
    let tail = store.get_range(key, 90, 20).unwrap().unwrap();
    assert_eq!(tail.bytes, bytes[90..]);
    assert_eq!(tail.total_len, 100);
    let whole = store.get_range(key, 0, MAX_GET_BYTES).unwrap().unwrap();
    assert_eq!(whole.bytes, bytes);
    assert_eq!(store.get_range(key, 100, 1), Err(StoreError::OutOfRange));
    assert_eq!(
        store.get_range(key, u64::MAX, 1),
        Err(StoreError::OutOfRange)
    );
    assert_eq!(store.get_range("js/job-1/stdout.log", 0, 10), Ok(None));
}

/// Asserts that deleting a key that does not exist succeeds and is an accepted operation, and
/// that a deleted key reads as missing.
#[test]
fn delete_of_a_missing_key_succeeds() {
    let rig = Rig::new();
    let store = rig.admin();
    store
        .delete("js/job-1/stdout/00000000000000000000")
        .unwrap();
    store
        .put("js/job-1/stdout.log", b"x".to_vec(), &Metadata::new())
        .unwrap();
    store.delete("js/job-1/stdout.log").unwrap();
    assert_eq!(store.get_range("js/job-1/stdout.log", 0, 1), Ok(None));
    assert_eq!(
        rig.fake.operations(),
        [
            Op::Delete {
                key: "js/job-1/stdout/00000000000000000000".to_owned()
            },
            Op::Put {
                key: "js/job-1/stdout.log".to_owned(),
                len: 1
            },
            Op::Delete {
                key: "js/job-1/stdout.log".to_owned()
            },
            Op::Get {
                key: "js/job-1/stdout.log".to_owned()
            },
        ]
    );
}

/// Pages through the listing of `prefix` after `start_after`, `max_keys` at a time, and returns
/// the keys and each page's length and truncation.
fn pages(
    store: &crate::support::FakeClient,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: u32,
) -> (Vec<String>, Vec<(usize, bool)>) {
    let mut keys: Vec<String> = Vec::new();
    let mut shapes = Vec::new();
    let mut after = start_after.map(str::to_owned);
    loop {
        let page = store.list(prefix, after.as_deref(), max_keys).unwrap();
        shapes.push((page.entries.len(), page.truncated));
        keys.extend(page.entries.iter().map(|entry| entry.key.clone()));
        if !page.truncated {
            return (keys, shapes);
        }
        after = keys.last().cloned();
    }
}

/// Asserts that 2,500 chunk keys come back in ascending order across pages of at most 1000,
/// `truncated` set on every page but the last, from a given `start_after` too, only under the
/// prefix (not a neighbour job's whose id begins the same), each with its size and the time it
/// was put; and that pages of 7 cross every edge without a duplicate or a skipped key. Catches
/// a duplicate or a skipped key at a page edge.
#[test]
fn list_pages_in_key_order_from_start_after() {
    let rig = Rig::new();
    let store = rig.admin();
    let job = JobPrefix::new("js", "job-1").unwrap();
    let neighbour = JobPrefix::new("js", "job-10").unwrap();
    let mut expected = Vec::new();
    for n in (0..2500_u64).rev() {
        let key = job.chunk_key(Stream::Stdout, n * 1000);
        store
            .put(
                &key,
                vec![b'x'; usize::try_from(n % 7 + 1).unwrap()],
                &Metadata::new(),
            )
            .unwrap();
        expected.push(key);
    }
    store
        .put(
            &neighbour.chunk_key(Stream::Stdout, 0),
            b"n".to_vec(),
            &Metadata::new(),
        )
        .unwrap();
    store
        .put(
            &job.final_key(Stream::Stdout),
            b"f".to_vec(),
            &Metadata::new(),
        )
        .unwrap();
    expected.sort();

    let prefix = job.stream_prefix(Stream::Stdout);
    let (keys, shapes) = pages(&store, &prefix, None, 1000);
    assert_eq!(keys, expected);
    assert_eq!(shapes, [(1000, true), (1000, true), (500, false)]);

    let from = expected[1233].clone();
    let (keys, shapes) = pages(&store, &prefix, Some(&from), 1000);
    assert_eq!(keys, expected[1234..]);
    assert_eq!(shapes, [(1000, true), (266, false)]);

    let (keys, _) = pages(&store, &prefix, None, 7);
    assert_eq!(keys, expected);

    let page = store.list(&prefix, None, 3).unwrap();
    let sizes: Vec<u64> = page.entries.iter().map(|entry| entry.size).collect();
    assert_eq!(sizes, [1, 2, 3]);
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.last_modified == at(START))
    );
    let last = store
        .list(&prefix, expected.last().map(String::as_str), 5)
        .unwrap();
    assert_eq!((last.entries.len(), last.truncated), (0, false));
}

/// Asserts that a credential expired by the client's clock is `Expired` before any request is
/// sent, that one expired by the store's clock only is `Expired` with the store's code, that
/// neither stores anything, and that a renewed credential set on the client works again.
#[test]
fn an_expired_credential_is_expired_not_denied() {
    let rig = Rig::new();
    let key = "js/job-1/stdout/00000000000000000000";
    let credential = rig.fake.issue(policy(&ALL, ""));
    let early = Arc::new(ManualClock::new(at(START)));
    let store_clock_only = Store::new(
        rig.fake.clone(),
        early,
        rig.endpoint.clone(),
        credential.clone(),
    );
    let store = rig.client(credential);

    rig.clock.advance(ISSUED_TTL);
    assert_eq!(
        store.put(key, b"late".to_vec(), &Metadata::new()),
        Err(StoreError::Expired {
            code: String::new()
        })
    );
    assert_eq!(
        store_clock_only.put(key, b"late".to_vec(), &Metadata::new()),
        Err(StoreError::Expired {
            code: "ExpiredToken".to_owned()
        })
    );
    assert_eq!(rig.fake.objects().len(), 0);
    assert!(rig.fake.operations().is_empty());

    store.set_credential(rig.fake.issue(policy(&ALL, "")));
    store
        .put(key, b"renewed".to_vec(), &Metadata::new())
        .unwrap();
    assert_eq!(rig.fake.objects()[key], b"renewed");
}

/// Asserts that with `lose_response_of_next_put` the put reports `Unavailable` and the object
/// exists, that a read in between is not affected, and that putting the same key and body
/// again succeeds and leaves the same object (a put replaces).
#[test]
fn a_lost_response_leaves_the_object_and_a_retry_is_safe() {
    let rig = Rig::new();
    let store = rig.admin();
    let key = "js/job-1/stdout/00000000000000000000";
    rig.fake.lose_response_of_next_put();
    assert_eq!(store.get_range(key, 0, 1), Ok(None));
    assert_eq!(
        store.put(key, b"chunk".to_vec(), &first_byte_at("7")),
        Err(StoreError::Unavailable(Unavailable::Transport(
            TransportError::Io(std::io::ErrorKind::ConnectionReset)
        )))
    );
    assert_eq!(rig.fake.objects()[key], b"chunk");
    store
        .put(key, b"chunk".to_vec(), &first_byte_at("7"))
        .unwrap();
    assert_eq!(rig.fake.objects()[key], b"chunk");
    let puts = rig
        .fake
        .operations()
        .into_iter()
        .filter(|op| matches!(op, Op::Put { .. }))
        .count();
    assert_eq!(puts, 2);
}

/// Asserts that each scripted failure is the error its status and code map to, that it answers
/// whatever the request and changes nothing, and that `fail_next(n, _)` fails exactly `n`
/// requests.
#[test]
fn scripted_failures_are_the_errors_their_codes_say() {
    let rig = Rig::new();
    let store = rig.admin();
    let key = "js/job-1/stdout/00000000000000000000";
    let unavailable = |status: u16, code: &str| {
        StoreError::Unavailable(Unavailable::Status {
            status,
            code: code.to_owned(),
        })
    };
    let cases = [
        (Status::SlowDown, unavailable(503, "SlowDown")),
        (Status::InternalError, unavailable(500, "InternalError")),
        (Status::TooManyRequests, unavailable(429, "TooManyRequests")),
        (Status::AccessDenied, denied()),
        (
            Status::ExpiredToken,
            StoreError::Expired {
                code: "ExpiredToken".to_owned(),
            },
        ),
    ];
    for (status, expected) in cases {
        rig.fake.fail_next(1, status);
        assert_eq!(
            store.put(key, b"x".to_vec(), &Metadata::new()),
            Err(expected),
            "{status:?}"
        );
    }
    rig.fake.fail_next(2, Status::SlowDown);
    assert_eq!(store.delete(key), Err(unavailable(503, "SlowDown")));
    assert_eq!(
        store.list("", None, 1).map(|page| page.truncated),
        Err(unavailable(503, "SlowDown"))
    );
    assert!(rig.fake.operations().is_empty());
    store.put(key, b"x".to_vec(), &Metadata::new()).unwrap();
    assert_eq!(rig.fake.objects().len(), 1);
}

/// Asserts that a held put waits, without any time limit, until the test releases it: the test
/// waits for the put to be held, sees nothing stored while a listing still goes through, then
/// releases it and sees it stored.
#[test]
fn a_held_put_waits_for_the_test() {
    let rig = Rig::new();
    let store = rig.admin();
    let key = "js/job-1/stdout.log";
    rig.fake.block_puts();
    thread::scope(|scope| {
        let put = scope.spawn(|| store.put(key, b"final".to_vec(), &Metadata::new()));
        rig.fake.wait_for_held_puts(1);
        assert_eq!(rig.fake.objects().len(), 0);
        assert_eq!(store.list("", None, 10).unwrap().entries.len(), 0);
        rig.fake.release_puts();
        assert_eq!(put.join().unwrap(), Ok(()));
    });
    assert_eq!(rig.fake.objects()[key], b"final");
    store.put(key, b"again".to_vec(), &Metadata::new()).unwrap();
    assert_eq!(rig.fake.objects()[key], b"again");
}

/// Asserts the limits checked before any request, at and past each bound: a body of
/// `MAX_PUT_BYTES` is put and one byte more is `TooLarge`; keys, metadata names and values,
/// ranges and listing sizes outside their bounds are `Invalid`; none of the refused calls
/// reaches the store.
#[test]
fn calls_outside_the_bounds_send_nothing() {
    let rig = Rig::new();
    let store = rig.admin();
    let key = "js/job-1/stdout.log";

    store
        .put(key, vec![0; MAX_PUT_BYTES], &Metadata::new())
        .unwrap();
    store.delete(key).unwrap();
    let before = rig.fake.operations();
    assert_eq!(
        store.put(key, vec![0; MAX_PUT_BYTES + 1], &Metadata::new()),
        Err(StoreError::TooLarge {
            len: MAX_PUT_BYTES + 1,
            max: MAX_PUT_BYTES
        })
    );

    let invalid = |result: Result<(), StoreError>| {
        assert!(matches!(result, Err(StoreError::Invalid(_))), "{result:?}");
    };
    let long_key = "k".repeat(1025);
    let long_name = "n".repeat(65);
    let long_value = "v".repeat(257);
    invalid(store.put("", b"x".to_vec(), &Metadata::new()));
    invalid(store.put(&long_key, b"x".to_vec(), &Metadata::new()));
    invalid(store.delete(""));
    invalid(store.get_range("", 0, 1).map(|_| ()));
    let refused_metadata = [
        ("", "1"),
        ("Upper", "1"),
        ("with_underscore", "1"),
        (long_name.as_str(), "1"),
        ("name", ""),
        ("name", "a b"),
        ("name", "tab\t"),
        ("name", "\u{e9}"),
        ("name", long_value.as_str()),
    ];
    for (name, value) in refused_metadata {
        let metadata = Metadata::from([(name.to_owned(), value.to_owned())]);
        invalid(store.put(key, b"x".to_vec(), &metadata));
    }
    invalid(store.get_range(key, 0, 0).map(|_| ()));
    invalid(store.get_range(key, 0, MAX_GET_BYTES + 1).map(|_| ()));
    invalid(store.get_range(key, u64::MAX, 2).map(|_| ()));
    invalid(store.list("", None, 0).map(|_| ()));
    invalid(store.list("", None, 1001).map(|_| ()));
    assert_eq!(rig.fake.operations(), before);

    let longest_key = "k".repeat(1024);
    let widest = Metadata::from([
        ("n".repeat(64), "v".repeat(256)),
        ("a-1".to_owned(), "~!".to_owned()),
    ]);
    store.put(&longest_key, b"x".to_vec(), &widest).unwrap();
    let object = store.get_range(&longest_key, 0, 1).unwrap().unwrap();
    assert_eq!(object.metadata, widest);
    assert_eq!(store.list("", None, 1000).unwrap().entries.len(), 1);
}
