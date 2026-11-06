//! The session policy of a host credential.
//!
//! Exactly one statement: `Effect` `Allow`, `Action` `s3:PutObject` and `s3:DeleteObject`, and
//! `Resource` `arn:aws:s3:::BUCKET/L*`, where `L` is the prefix given with its trailing `/`
//! (`arn:aws:s3:::BUCKET/*` for an empty one). The minters give the host's prefix
//! ([`crate::MintRequest::host_prefix`]: the configured log prefix and `js-<32 hex>/`), so a
//! host's policy covers its own jobs and no other host's. Because `L` ends in `/`, the pattern
//! of the prefix `a/b` matches `a/b/...` and never `a/bc/...`. The trailing `*` is the
//! resource's only wildcard: the bucket and the prefix are checked ([`crate::names`]) so that
//! they hold no `*`, `?`, `$` or `{`, and no character that JSON would have to escape. There is
//! no other action, no `NotResource` and no condition.
//!
//! The text is built with `serde_json`, so whatever the resource held it could not end the
//! string or add a member. For any bucket and prefix the names allow it is under the 2,048
//! characters STS takes.

use serde_json::json;

use crate::names::{Bucket, LogPrefix};

/// The version of the IAM policy language every session policy is written in. A third-party
/// constant: the identifier AWS defines for the current version of the language, which has the
/// form of a calendar date and must be sent exactly as AWS spells it.
pub const POLICY_VERSION: &str = "2012-10-17";

/// The session policy that allows `s3:PutObject` and `s3:DeleteObject` on the objects of
/// `bucket` under `prefix` (a host's prefix, as the minters give it), and nothing else.
pub fn policy(bucket: &Bucket, prefix: &LogPrefix) -> String {
    document(&format!(
        "arn:aws:s3:::{}/{}*",
        bucket.as_str(),
        prefix.as_str()
    ))
}

/// The policy document of one statement that allows putting and deleting `resource`.
fn document(resource: &str) -> String {
    json!({
        "Version": POLICY_VERSION,
        "Statement": [{
            "Effect": "Allow",
            "Action": ["s3:PutObject", "s3:DeleteObject"],
            "Resource": resource,
        }],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// Asserts that the builder keeps a resource in its string whatever it holds: a quote, a
    /// backslash, a brace and a control character come back exactly when the text is parsed,
    /// and the document still has its two members and one statement. The names never let such
    /// a character reach the builder; this shows that the text cannot be broken even if one
    /// did. It catches a policy built by pasting text into a template.
    #[test]
    fn the_resource_cannot_break_out_of_its_string() {
        let hostile = "arn:aws:s3:::b/\"},{\"Effect\":\"Allow\",\"Resource\":\"*\\\n{x}*";
        let text = document(hostile);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 2);
        let statements = parsed["Statement"].as_array().unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0]["Resource"], hostile);
        assert_eq!(statements[0].as_object().unwrap().len(), 3);
    }
}
