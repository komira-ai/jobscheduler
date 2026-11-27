//! tierstore's upstream store: an S3 server of the workflow's, which the test puts the jobs'
//! input tree into, straight (not through tierstore), as a producer of trees does.
//!
//! The tree is a version 1 tree list (tierstore's `docs/tree-manifest.md`): a directory and one
//! empty file, so it needs no object but the list. Its key is `cas/l/<SHA-256 of the list>`,
//! the ref a job names.

use std::net::SocketAddr;
use std::time::SystemTime;

use hmac::{Hmac, KeyInit, Mac as _};
use sha2::{Digest as _, Sha256};

use crate::http;

/// The bucket tierstore's cold store names, which the workflow's S3 server makes.
pub const BUCKET: &str = "cas";

/// The region the S3 server signs in, and tierstore's cold store's default.
const REGION: &str = "us-east-1";

/// The file the jobs check for, under their mount.
pub const READY_FILE: &str = "input/ready";

/// The tree list's bytes.
fn tree_list() -> Vec<u8> {
    concat!(
        r#"{"format":"tierstore.tree-list","version":1}"#,
        "\n",
        r#"{"path":"input","kind":"dir","mode":493}"#,
        "\n",
        r#"{"path":"input/ready","kind":"file","mode":420,"size":0}"#,
        "\n",
    )
    .as_bytes()
    .to_vec()
}

/// The store's credentials.
pub struct Credentials {
    pub key_id: String,
    pub secret: String,
}

/// Puts the tree into the store at `store` and returns its ref.
pub fn put_tree(store: SocketAddr, credentials: &Credentials) -> String {
    let list = tree_list();
    let key = format!("cas/l/{}", hex::encode(Sha256::digest(&list)));
    put(store, credentials, &key, &list);
    key
}

/// `PUT /<BUCKET>/<key>` of `body`, signed with AWS Signature Version 4.
fn put(store: SocketAddr, credentials: &Credentials, key: &str, body: &[u8]) {
    let (date, stamp) = amz_date(SystemTime::now());
    let payload = hex::encode(Sha256::digest(body));
    let path = format!("/{BUCKET}/{key}");
    let host = store.to_string();
    let signed = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "PUT\n{path}\n\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\n{signed}\n{payload}"
    );
    let scope = format!("{date}/{REGION}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let mut signing = format!("AWS4{}", credentials.secret).into_bytes();
    for part in [date.as_str(), REGION, "s3", "aws4_request"] {
        signing = hmac(&signing, part.as_bytes());
    }
    let signature = hex::encode(hmac(&signing, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        credentials.key_id
    );
    let answer = http::request(
        store,
        "PUT",
        &path,
        &[
            ("x-amz-content-sha256", payload),
            ("x-amz-date", stamp),
            ("authorization", authorization),
        ],
        body,
    )
    .unwrap_or_else(|e| panic!("put {key}: {e}"));
    assert_eq!(answer.status, 200, "put {key}: {}", answer.text());
}

/// HMAC-SHA256 of `data` under `key`.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The day (`YYYYMMDD`) and the instant (`YYYYMMDDTHHMMSSZ`) of `now`, in UTC.
fn amz_date(now: SystemTime) -> (String, String) {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    let days = i64::try_from(secs / 86_400).expect("a day count");
    let rest = secs % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}{month:02}{day:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    (date, stamp)
}
