//! What a callback carries besides its URL (`docs/design/job-retry-and-completion-callback.md`,
//! sections 5.5 and 5.6): the `webhook-signature` value ([`sign`]), in the Standard Webhooks
//! form, and the opaque `webhook-id` the store mints for a job's callback once, at submit
//! ([`mint_webhook_id`]).
//!
//! The signature is `v1,` and the standard base64 of HMAC-SHA256 under the key's raw bytes of
//! `id "." timestamp "." body`. The body is always empty, so the signed text ends at the second
//! dot. A receiver's Standard Webhooks library takes the key as `whsec_` followed by the
//! standard base64 of those bytes.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::SigningKey;

/// How many bytes from the server's id source a `webhook-id` is minted from.
pub const WEBHOOK_ID_BYTES: usize = 16;

/// The `webhook-signature` value of a call with `webhook_id` at `timestamp` (seconds since the
/// epoch) and an empty body, signed with `key`: `v1,<base64 HMAC-SHA256(key, id "." timestamp
/// ".")>`.
#[must_use]
pub fn sign(key: &SigningKey, webhook_id: &str, timestamp: u64) -> String {
    signature(key.as_bytes(), webhook_id, timestamp, b"")
}

/// The Standard Webhooks signature of `body` sent with `webhook_id` at `timestamp`, under
/// `key`. HMAC takes a key of any length (`new_from_slice` never refuses one), so the empty
/// text of a refusal is never returned.
fn signature(key: &[u8], webhook_id: &str, timestamp: u64, body: &[u8]) -> String {
    let keyed = <Hmac<Sha256> as KeyInit>::new_from_slice(key);
    keyed
        .map(|mut mac| {
            mac.update(format!("{webhook_id}.{timestamp}.").as_bytes());
            mac.update(body);
            let digest: [u8; 32] = mac.finalize().into_bytes().into();
            format!("v1,{}", STANDARD.encode(digest))
        })
        .unwrap_or_default()
}

/// The `webhook-id` of a job's callback, minted from `random`, [`WEBHOOK_ID_BYTES`] bytes the
/// caller draws from its id source: their base64url, with no padding. It is made of those
/// bytes alone, so it says nothing of the job, and a job id used again gets another.
#[must_use]
pub fn mint_webhook_id(random: [u8; WEBHOOK_ID_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(random)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes 0, 1, 2 and so on, `len` of them.
    fn counting(len: u8) -> Vec<u8> {
        (0..len).collect()
    }

    /// Asserts three signatures against values a separate HMAC-SHA256 implementation (Python's
    /// `hmac`) computed: the Standard Webhooks specification's own test vector, whose secret is
    /// `whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw` and whose body is not empty; an empty body under
    /// the shortest key; and an empty body under the longest key, which fills an HMAC block.
    /// Catches a signed text that is not `id.timestamp.body` (a missing trailing dot for an
    /// empty body, say), a key encoded before it is used, a URL-safe alphabet in place of the
    /// standard one, a missing `v1,`, and a padding of the key that differs from HMAC's.
    #[test]
    fn signature_matches_the_reference_for_an_empty_body() {
        let spec_key = STANDARD
            .decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")
            .expect("the specification's secret");
        assert_eq!(
            signature(
                &spec_key,
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1_614_265_330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
        let shortest = SigningKey::new(counting(32)).expect("a key");
        assert_eq!(
            sign(&shortest, "msg_2KWPBgLlAfxdpx2AI54pPJ85f4W", 1_798_761_600),
            "v1,2YmkLa/YKkNC4uexQssZVwnzHzOjtQlyypa8SU0FrjI="
        );
        let longest = SigningKey::new(counting(64)).expect("a key");
        assert_eq!(
            sign(&longest, "AAECAwQFBgcICQoLDA0ODw", 4_102_444_800),
            "v1,ug4avuwX87kotpVr757uFh4pnDXf8ideJ6MmAaEKWpI="
        );
    }

    /// Asserts the whole `webhook-id` minted from two draws, the bytes 0 to 15 and sixteen
    /// 0xff bytes (which use the two URL-safe letters and no padding), and that two draws for
    /// one job id give two ids, neither holding the job id. Catches an id derived from the job,
    /// the standard alphabet (whose `+` and `/` a receiver's path or log could mangle), and
    /// padding.
    #[test]
    fn the_webhook_id_reveals_nothing_of_the_job() {
        let first: [u8; WEBHOOK_ID_BYTES] = counting(16).try_into().expect("16 bytes");
        assert_eq!(mint_webhook_id(first), "AAECAwQFBgcICQoLDA0ODw");
        assert_eq!(mint_webhook_id([0xff; 16]), "_____________________w");
        // The same job id, submitted again after its retention ended: a new draw.
        let job = "job-7";
        let (before, again) = (mint_webhook_id(first), mint_webhook_id([7; 16]));
        assert_ne!(before, again);
        assert_eq!([before.contains(job), again.contains(job)], [false, false]);
    }
}
