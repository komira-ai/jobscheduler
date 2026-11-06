//! Put, ranged get, list and delete of a job's chunk objects with the root key: each request is
//! signed by this crate's signer and checked by the store, and each answer read by this crate's
//! parsers.

use js_logstore::layout::FIRST_BYTE_AT_MS;
use js_logstore::{MAX_CHUNK_BYTES, MAX_GET_BYTES, Metadata, Object, StoreError, Stream};

use super::support::{Rig, every_byte};

/// The metadata of a chunk whose first byte was seen at `ms`.
fn first_byte_at(ms: u64) -> Metadata {
    Metadata::from([(FIRST_BYTE_AT_MS.to_owned(), ms.to_string())])
}

/// Proves a chunk put with its metadata reads back byte for byte over every kind of range: the
/// whole object, one in the middle, one that runs past the end (fewer bytes, the whole length
/// still told), the last byte alone; a range that starts at the end is `OutOfRange`, and a key
/// never put is `None`. The metadata comes back as it was put on every read. Catches a ranged
/// read whose header, `content-range` reading or metadata reading differs from what a real
/// store does, and a body signed with a payload hash other than the bytes sent.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_chunk_reads_back_over_every_kind_of_range_with_its_metadata() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let job = rig.job(&rig.prefix, "job-1");
    let key = job.chunk_key(Stream::Stdout, 0);
    let body = every_byte(3000);
    let metadata = first_byte_at(1_234_567);
    root.put(&key, body.clone(), &metadata).expect("put");

    let read = |start: u64, len: u64| root.get_range(&key, start, len);
    let object = |bytes: &[u8]| {
        Ok(Some(Object {
            bytes: bytes.to_vec(),
            metadata: metadata.clone(),
            total_len: 3000,
        }))
    };
    assert_eq!(read(0, 3000), object(&body));
    assert_eq!(read(1000, 500), object(&body[1000..1500]));
    assert_eq!(read(2900, 500), object(&body[2900..]));
    assert_eq!(read(2999, 1), object(&body[2999..]));
    assert_eq!(read(3000, 1), Err(StoreError::OutOfRange));
    let missing = job.chunk_key(Stream::Stdout, 3000);
    assert_eq!(root.get_range(&missing, 0, 1), Ok(None));
}

/// Proves the largest bodies the uploader puts and the reader reads go through: a chunk of
/// `MAX_CHUNK_BYTES` holding every byte value, read back whole in one range of
/// `MAX_GET_BYTES`, and an empty object, whose every range is `OutOfRange`. Catches a payload
/// hash that differs from the store's for a long body, and a read cap that cuts a full chunk.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_full_chunk_and_an_empty_object_round_trip() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let job = rig.job(&rig.prefix, "job-1");
    let full = job.chunk_key(Stream::Stdout, 0);
    let body = every_byte(MAX_CHUNK_BYTES);
    root.put(&full, body.clone(), &Metadata::new())
        .expect("put");
    let read = root
        .get_range(&full, 0, MAX_GET_BYTES)
        .expect("read")
        .expect("there");
    assert_eq!(read.total_len, MAX_GET_BYTES);
    assert!(read.bytes == body, "the full chunk came back changed");

    let empty = job.final_key(Stream::Stderr);
    root.put(&empty, Vec::new(), &Metadata::new()).expect("put");
    assert_eq!(root.get_range(&empty, 0, 1), Err(StoreError::OutOfRange));
}

/// Proves a key with characters a path must encode (a space, `+`, `=`, `%`, `~`, `é`) is put,
/// read and listed under its own name. Catches a canonical URI in the signature that is not
/// the one the store computes from the path it was sent.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_key_the_path_must_encode_is_signed_as_the_store_reads_it() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let key = format!("{}odd key+a=b%c~é", rig.prefix.as_str());
    root.put(&key, b"odd".to_vec(), &Metadata::new())
        .expect("put");
    let read = root.get_range(&key, 0, 3).expect("read").expect("there");
    assert_eq!(read.bytes, b"odd");
    let page = root.list(rig.prefix.as_str(), None, 10).expect("list");
    let keys: Vec<&str> = page.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, [key.as_str()]);
}

/// Proves a listing of a stream's chunks comes back in key order, which is offset order (the
/// offsets are zero-padded), with each object's size; a page of two is truncated and the next
/// page starts after its last key; a page after the last key is empty and not truncated, and
/// so is a listing of a prefix with nothing under it. Catches a list query signed or encoded
/// other than the store reads it, and a page scan that breaks on a real store's answer.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_listing_pages_through_the_chunks_in_offset_order() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let job = rig.job(&rig.prefix, "job-1");
    let offsets = [0_u64, 10, 1 << 40];
    for (index, offset) in offsets.iter().enumerate().rev() {
        let key = job.chunk_key(Stream::Stdout, *offset);
        root.put(&key, every_byte(index + 1), &first_byte_at(*offset))
            .expect("put");
    }
    root.put(
        &job.final_key(Stream::Stdout),
        b"whole".to_vec(),
        &Metadata::new(),
    )
    .expect("put the final");
    let prefix = job.stream_prefix(Stream::Stdout);
    let all = root.list(&prefix, None, 1000).expect("list");
    let listed: Vec<(String, u64)> = all
        .entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.size))
        .collect();
    let want: Vec<(String, u64)> = offsets
        .iter()
        .enumerate()
        .map(|(index, offset)| (job.chunk_key(Stream::Stdout, *offset), index as u64 + 1))
        .collect();
    assert_eq!(listed, want);
    assert!(!all.truncated);

    let first = root.list(&prefix, None, 2).expect("a page of two");
    assert!(first.truncated);
    assert_eq!(first.entries.len(), 2);
    let after = first.entries[1].key.clone();
    let rest = root.list(&prefix, Some(&after), 2).expect("the next page");
    let rest_keys: Vec<&str> = rest.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(rest_keys, [want[2].0.as_str()]);
    assert!(!rest.truncated);
    let past = root
        .list(&prefix, Some(&want[2].0), 2)
        .expect("past the end");
    assert!(past.entries.is_empty() && !past.truncated);
    let nothing = rig.job(&rig.prefix, "job-2").stream_prefix(Stream::Stdout);
    let none = root.list(&nothing, None, 1000).expect("an empty listing");
    assert!(none.entries.is_empty() && !none.truncated);
}

/// Proves a delete removes the object, so a read finds nothing and a listing leaves it out,
/// and a delete of a key that is not there is success, as the uploader's retried deletes need.
/// Catches a delete signed other than the store reads it, and a missing key answered as a
/// failure.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_delete_removes_the_chunk_and_a_second_delete_is_success() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let job = rig.job(&rig.prefix, "job-1");
    let gone = job.chunk_key(Stream::Stderr, 0);
    let kept = job.chunk_key(Stream::Stderr, 5);
    root.put(&gone, b"first".to_vec(), &Metadata::new())
        .expect("put");
    root.put(&kept, b"second".to_vec(), &Metadata::new())
        .expect("put");
    root.delete(&gone).expect("delete");
    assert_eq!(root.get_range(&gone, 0, 1), Ok(None));
    let page = root
        .list(&job.stream_prefix(Stream::Stderr), None, 10)
        .expect("list");
    let keys: Vec<&str> = page.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, [kept.as_str()]);
    root.delete(&gone)
        .expect("a missing key deletes as success");
}
