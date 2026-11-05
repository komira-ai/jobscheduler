//! Published AWS Signature Version 4 test vectors, copied verbatim.
//!
//! This file is test data, not ours: the calendar dates, time stamps and `Date` header in it
//! (the `date`, `stamp` and `unix_seconds` fields, and the `date` header of the PUT example)
//! are the vectors' own fixed inputs, third-party protocol identifiers that the published
//! signatures depend on. It is the only file of the crate that holds them.
//!
//! - The generic suite (service `service`, host `example.amazonaws.com`): `get-vanilla`,
//!   `get-vanilla-query`, `post-vanilla`, `get-header-value-trim`, `get-utf8`,
//!   `get-space-unnormalized` and the session-token vector `post-sts-header-before`. Its
//!   requests' paths are the raw paths, which the generic form encodes.
//! - The worked examples of the S3 documentation for header authentication (service `s3`, host
//!   `examplebucket.s3.amazonaws.com`): GET Object with a `Range` header, PUT Object, and GET
//!   Bucket (list objects). Their paths are as sent, which the S3 form does not encode again.

/// One vector: the request, the credential and the published result.
pub struct Vector {
    pub name: &'static str,
    pub unix_seconds: u64,
    pub date: &'static str,
    pub stamp: &'static str,
    pub access_key_id: &'static str,
    pub secret_access_key: &'static str,
    pub session_token: Option<&'static str>,
    pub region: &'static str,
    pub service: &'static str,
    pub method: &'static str,
    pub host: &'static str,
    pub path: &'static str,
    pub query: &'static [(&'static str, &'static str)],
    pub headers: &'static [(&'static str, &'static str)],
    pub sign_also: &'static [&'static str],
    pub body: &'static [u8],
    pub signed_headers: &'static str,
    pub signature: &'static str,
}

impl Vector {
    /// The `Authorization` header the vector's signature makes.
    pub fn authorization(&self) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders={}, Signature={}",
            self.access_key_id,
            self.date,
            self.region,
            self.service,
            self.signed_headers,
            self.signature
        )
    }
}

const SUITE_TIME: u64 = 1_440_938_160;
const SUITE_DATE: &str = "20150830";
const SUITE_STAMP: &str = "20150830T123600Z";
const SUITE_KEY_ID: &str = "AKIDEXAMPLE";
const SUITE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const SUITE_HOST: &str = "example.amazonaws.com";

const S3_TIME: u64 = 1_369_353_600;
const S3_DATE: &str = "20130524";
const S3_STAMP: &str = "20130524T000000Z";
const S3_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const S3_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const S3_HOST: &str = "examplebucket.s3.amazonaws.com";

/// The `Date` header of the S3 PUT Object example.
pub const PUT_DATE_HEADER: &str = "Fri, 24 May 2013 00:00:00 GMT";

/// The session token of `post-sts-header-before`.
const SUITE_TOKEN: &str = "AQoDYXdzEPT//////////wEXAMPLEtc764bNrC9SAPBSM22wDOk4x4HIZ8j4FZTwdQWLWsKWHGBuFqwAeMicRXmxfpSPfIeoIYRqTflfKD8YUuwthAx7mSEI/qkPpKPi/kMcGdQrmGdeehM4IC1NtBmUpp2wUE8phUZampKsburEDy0KPkyQDYwT7WZ0wq5VSXDvp75YU9HFvlRd8Tx6q6fE8YQcHNVXAkiY9q6d+xo0rKwT38xVqr7ZD0u0iPPkUL64lIZbqBAz+scqKmlzm8FDrypNC9Yjc8fPOLn9FX9KSYvKTr4rvx3iSIlTJabIQwj2ICCR/oLxBA==";

/// A generic-suite vector with the suite's credential and time.
const fn suite(
    name: &'static str,
    method: &'static str,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    sign_also: &'static [&'static str],
    signed_headers: &'static str,
    signature: &'static str,
) -> Vector {
    Vector {
        name,
        unix_seconds: SUITE_TIME,
        date: SUITE_DATE,
        stamp: SUITE_STAMP,
        access_key_id: SUITE_KEY_ID,
        secret_access_key: SUITE_SECRET,
        session_token: None,
        region: "us-east-1",
        service: "service",
        method,
        host: SUITE_HOST,
        path,
        query: &[],
        headers,
        sign_also,
        body: b"",
        signed_headers,
        signature,
    }
}

/// An S3-documentation example with its credential and time.
#[allow(clippy::too_many_arguments)] // Reason: one argument per field the examples differ in.
const fn s3(
    name: &'static str,
    method: &'static str,
    path: &'static str,
    query: &'static [(&'static str, &'static str)],
    headers: &'static [(&'static str, &'static str)],
    sign_also: &'static [&'static str],
    body: &'static [u8],
    signed_headers: &'static str,
    signature: &'static str,
) -> Vector {
    Vector {
        name,
        unix_seconds: S3_TIME,
        date: S3_DATE,
        stamp: S3_STAMP,
        access_key_id: S3_KEY_ID,
        secret_access_key: S3_SECRET,
        session_token: None,
        region: "us-east-1",
        service: "s3",
        method,
        host: S3_HOST,
        path,
        query,
        headers,
        sign_also,
        body,
        signed_headers,
        signature,
    }
}

/// Every vector.
pub const VECTORS: [Vector; 10] = [
    suite(
        "get-vanilla",
        "GET",
        "/",
        &[],
        &[],
        "host;x-amz-date",
        "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
    ),
    suite(
        "get-vanilla-query",
        "GET",
        "/",
        &[],
        &[],
        "host;x-amz-date",
        "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
    ),
    suite(
        "post-vanilla",
        "POST",
        "/",
        &[],
        &[],
        "host;x-amz-date",
        "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
    ),
    suite(
        "get-header-value-trim",
        "GET",
        "/",
        &[("My-Header1", " value1"), ("My-Header2", " \"a   b   c\"")],
        &["my-header1", "my-header2"],
        "host;my-header1;my-header2;x-amz-date",
        "acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736",
    ),
    suite(
        "get-utf8",
        "GET",
        "/\u{1234}",
        &[],
        &[],
        "host;x-amz-date",
        "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
    ),
    suite(
        "get-space-unnormalized",
        "GET",
        "/example space/",
        &[],
        &[],
        "host;x-amz-date",
        "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741",
    ),
    Vector {
        session_token: Some(SUITE_TOKEN),
        ..suite(
            "post-sts-header-before",
            "POST",
            "/",
            &[],
            &[],
            "host;x-amz-date;x-amz-security-token",
            "85d96828115b5dc0cfc3bd16ad9e210dd772bbebba041836c64533a82be05ead",
        )
    },
    s3(
        "s3-get-object-range",
        "GET",
        "/test.txt",
        &[],
        &[("range", "bytes=0-9")],
        &["range"],
        b"",
        "host;range;x-amz-content-sha256;x-amz-date",
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
    ),
    s3(
        "s3-put-object",
        "PUT",
        "/test%24file.text",
        &[],
        &[
            ("date", PUT_DATE_HEADER),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ],
        &["date"],
        b"Welcome to Amazon S3.",
        "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
        "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
    ),
    s3(
        "s3-list-objects",
        "GET",
        "/",
        &[("max-keys", "2"), ("prefix", "J")],
        &[],
        &[],
        b"",
        "host;x-amz-content-sha256;x-amz-date",
        "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
    ),
];

/// The canonical request of `get-vanilla`, as the suite publishes it.
pub const GET_VANILLA_CANONICAL_REQUEST: &str = "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
