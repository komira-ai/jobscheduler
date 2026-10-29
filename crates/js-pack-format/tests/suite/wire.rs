//! The grant, the report and their encoding.

use js_pack_format::layout::{MAX_BLOBS_PER_PACK, MAX_PACK_BYTES, OWN_PACK_BLOB_BYTES};
use js_pack_format::{
    Digest, PackEntry, PackLimits, PackReport, UploadGrant, UploadOutcome, UploadSlot,
};

const SECRET: &str = "https://store.example/cas/b/x/n-0?X-Amz-Signature=deadbeef";

fn grant() -> UploadGrant {
    UploadGrant {
        slots: vec![
            UploadSlot {
                key: "cas/b/x/n-0".to_string(),
                put_url: SECRET.to_string(),
            },
            UploadSlot {
                key: "cas/b/x/n-1".to_string(),
                put_url: format!("{SECRET}2"),
            },
        ],
        max_pack_bytes: 1 << 20,
        max_blobs_per_pack: 100,
        expires_unix_ms: 1_000,
    }
}

/// Catches: a grant or slot whose Debug (or pretty Debug, or Debug inside another value)
/// prints a URL or its signature, which would put a write credential in a log or an error.
#[test]
fn debug_of_a_grant_shows_no_url() {
    let g = grant();
    assert_eq!(format!("{g:?}"), "<upload grant: 2 slots>");
    assert_eq!(format!("{g:#?}"), "<upload grant: 2 slots>");
    let nested = format!("{:?}", Some(g.clone()));
    assert_eq!(nested, "Some(<upload grant: 2 slots>)");

    let slot = format!("{:?}", g.slots[0]);
    assert!(slot.contains("cas/b/x/n-0"), "{slot}");
    assert!(slot.contains("<redacted>"), "{slot}");
    for text in [&slot, &nested] {
        assert!(!text.contains("store.example"), "{text}");
        assert!(!text.contains("deadbeef"), "{text}");
    }
}

/// Catches: a grant that asks for a larger pack than a store accepts being passed on,
/// and a smaller one being ignored.
#[test]
fn a_grant_cannot_raise_the_limits() {
    let g = grant();
    assert_eq!(
        g.limits(),
        PackLimits {
            max_bytes: 1 << 20,
            max_blobs: 100,
            own_pack_bytes: OWN_PACK_BLOB_BYTES,
        }
    );
    let greedy = UploadGrant {
        max_pack_bytes: u64::MAX,
        max_blobs_per_pack: u32::MAX,
        ..g
    };
    let limits = greedy.limits();
    assert_eq!(limits.max_bytes, MAX_PACK_BYTES);
    assert_eq!(limits.max_blobs, MAX_BLOBS_PER_PACK);
}

/// Catches: a changed field name, tag, case or digest encoding on the wire. The JSON is
/// written out: a digest is 64 lowercase hex digits, a variant is its snake_case name.
#[test]
fn the_encoding_is_pinned() {
    let entry = PackEntry {
        digest: [0xab; 32],
        size: 5,
        offset: 7,
    };
    let hex = "ab".repeat(32);
    let entry_json = format!(r#"{{"digest":"{hex}","size":5,"offset":7}}"#);
    assert_eq!(serde_json::to_string(&entry).unwrap(), entry_json);

    let report = PackReport {
        key: "cas/b/x/n-0".to_string(),
        size: 99,
        manifest: vec![entry],
    };
    let report_json = format!(r#"{{"key":"cas/b/x/n-0","size":99,"manifest":[{entry_json}]}}"#);
    assert_eq!(serde_json::to_string(&report).unwrap(), report_json);

    let cases = [
        (
            UploadOutcome::Uploaded(vec![report]),
            format!(r#"{{"uploaded":[{report_json}]}}"#),
        ),
        (
            UploadOutcome::Uploaded(vec![]),
            r#"{"uploaded":[]}"#.to_string(),
        ),
        (
            UploadOutcome::NothingToUpload,
            r#""nothing_to_upload""#.to_string(),
        ),
        (
            UploadOutcome::TooLarge {
                needed_slots: 3,
                needed_bytes: 9,
            },
            r#"{"too_large":{"needed_slots":3,"needed_bytes":9}}"#.to_string(),
        ),
        (
            UploadOutcome::GrantExpired,
            r#""grant_expired""#.to_string(),
        ),
        (
            UploadOutcome::UploadRefused { status: 403 },
            r#"{"upload_refused":{"status":403}}"#.to_string(),
        ),
    ];
    for (outcome, json) in cases {
        assert_eq!(serde_json::to_string(&outcome).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<UploadOutcome>(&json).unwrap(),
            outcome
        );
    }
}

/// Catches: a grant that does not survive the wire, and a decoder that rejects a field a
/// later version adds (the encoding is additive).
#[test]
fn a_grant_round_trips_and_ignores_unknown_fields() {
    let g = grant();
    let json = serde_json::to_string(&g).unwrap();
    assert!(json.contains(SECRET), "the wire carries the URL");
    assert_eq!(serde_json::from_str::<UploadGrant>(&json).unwrap(), g);

    let extended = json.replacen('{', r#"{"added_later":1,"#, 1);
    assert_eq!(serde_json::from_str::<UploadGrant>(&extended).unwrap(), g);
}

/// Catches: a digest decoder that accepts text that is not one hash (wrong length, upper
/// case, a non-hex digit in either half of a byte, a number), or an error that repeats
/// the peer's text. Every hex digit value round-trips.
#[test]
fn digests_are_64_lowercase_hex_digits() {
    let all: [u8; 32] = std::array::from_fn(|i| (i * 8 + 3) as u8);
    let entry = PackEntry {
        digest: all,
        size: 0,
        offset: 0,
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert_eq!(serde_json::from_str::<PackEntry>(&json).unwrap(), entry);
    let digits: String = all.iter().map(|b| format!("{b:02x}")).collect();
    assert!(json.contains(&digits));

    let ok = "00".repeat(32);
    let bad_texts = [
        "ab".repeat(31),
        format!("g{}", "0".repeat(63)),
        format!("0g{}", "0".repeat(62)),
        "AB".repeat(32),
        "/!".repeat(32),
    ];
    for text in bad_texts {
        let json = format!(r#"{{"digest":"{text}","size":0,"offset":0}}"#);
        let err = serde_json::from_str::<PackEntry>(&json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("64 lowercase hex digits"), "{err}");
        assert!(!err.contains(&text), "{err}");
    }
    let json = r#"{"digest":5,"size":0,"offset":0}"#;
    assert!(serde_json::from_str::<PackEntry>(json).is_err());
    let json = format!(r#"{{"digest":"{ok}","size":0,"offset":0}}"#);
    assert!(serde_json::from_str::<PackEntry>(&json).is_ok());
}

/// Catches: a digest whose text form or order drifted: the text is `<hex>/<size>`, and
/// order is by hash, then size.
#[test]
fn a_digest_prints_and_orders() {
    assert_eq!(
        Digest::of(b"").to_string(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855/0"
    );
    let small = Digest {
        hash: [1; 32],
        size_bytes: 1,
    };
    let large = Digest {
        hash: [1; 32],
        size_bytes: 2,
    };
    let other = Digest {
        hash: [2; 32],
        size_bytes: 0,
    };
    assert!(small < large && large < other);
}
