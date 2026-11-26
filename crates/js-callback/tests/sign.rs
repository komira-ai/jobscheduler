//! The `webhook-signature` of a callback (`docs/design/job-retry-and-completion-callback.md`,
//! section 5.6) against reference values a separate HMAC-SHA256 implementation (Python's
//! `hmac`) computed. The vectors live here, under `tests/`, as the repository keeps its other
//! signing vectors (js-logstore's `tests/logstore/signer.rs`).

use js_callback::{SigningKey, sign};

/// The bytes 0, 1, 2 and so on, `len` of them.
fn counting(len: u8) -> Vec<u8> {
    (0..len).collect()
}

/// Asserts the signature of an empty body under the shortest key (32 bytes) and the longest
/// (64 bytes, a whole HMAC block), each against its reference value. Catches a signed text
/// that does not end at the second dot for an empty body, a key padded or hashed otherwise
/// than HMAC does, the URL-safe alphabet in place of the standard one, and a missing `v1,`.
#[test]
fn signature_matches_the_reference_for_an_empty_body() {
    let shortest = SigningKey::new(counting(32)).expect("a key");
    let longest = SigningKey::new(counting(64)).expect("a key");
    let signed = [
        sign(&shortest, "msg_2KWPBgLlAfxdpx2AI54pPJ85f4W", 1_798_761_600),
        sign(&longest, "AAECAwQFBgcICQoLDA0ODw", 4_102_444_800),
    ];
    assert_eq!(
        signed,
        [
            "v1,2YmkLa/YKkNC4uexQssZVwnzHzOjtQlyypa8SU0FrjI=",
            "v1,ug4avuwX87kotpVr757uFh4pnDXf8ideJ6MmAaEKWpI=",
        ]
    );
}
