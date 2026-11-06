//! The S3 client: put, delete, ranged get and list, each one signed request over a
//! [`Transport`], never retried.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use crate::clock::Clock;
use crate::credential::{Credential, Endpoint};
use crate::error::{StoreError, missing_key, refusal, transport};
use crate::http::{HttpRequest, HttpResponse, Method, Transport};
use crate::layout::MAX_PUT_BYTES;
use crate::list::{Page, parse_page};
use crate::sigv4::{Scope, Unsigned, canonical_query, sign};

/// The most bytes one ranged read asks for: a page of a chunk or a final is well under it.
pub const MAX_GET_BYTES: u64 = 1_048_576;

/// The most keys one listing asks for (S3's own ceiling).
pub const MAX_LIST_KEYS: u32 = 1000;

/// The longest key, in bytes (S3's own ceiling).
pub const MAX_KEY_LEN: usize = 1024;

/// The longest metadata name, in bytes, after `x-amz-meta-`.
pub const MAX_METADATA_NAME_LEN: usize = 64;

/// The longest metadata value, in bytes.
pub const MAX_METADATA_VALUE_LEN: usize = 256;

/// The header prefix of an object's metadata.
const META: &str = "x-amz-meta-";

/// An object's metadata, by the name after `x-amz-meta-` (lower case).
pub type Metadata = BTreeMap<String, String>;

/// What a ranged read returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The bytes of the range that exist: fewer than asked when the object ends first.
    pub bytes: Vec<u8>,
    /// The object's metadata.
    pub metadata: Metadata,
    /// The whole object's length.
    pub total_len: u64,
}

/// A client of one bucket, signing with one credential at a time. It makes exactly one request
/// per call and never retries: the caller owns retry and backoff. It reads no environment
/// variable and no file.
pub struct Store<T, C> {
    transport: T,
    clock: C,
    endpoint: Endpoint,
    credential: Mutex<Credential>,
}

impl<T, C> fmt::Debug for Store<T, C> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Store")
            .field("endpoint", &self.endpoint)
            .field("credential", &*self.credential())
            .finish_non_exhaustive()
    }
}

impl<T, C> Store<T, C> {
    /// The credential, under its lock.
    fn credential(&self) -> MutexGuard<'_, Credential> {
        self.credential
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Transport, C: Clock> Store<T, C> {
    /// A client of `endpoint`'s bucket over `transport`, signing with `credential` at the times
    /// `clock` reads.
    pub fn new(transport: T, clock: C, endpoint: Endpoint, credential: Credential) -> Self {
        Self {
            transport,
            clock,
            endpoint,
            credential: Mutex::new(credential),
        }
    }

    /// Signs every later request with `credential` (a renewed one).
    pub fn set_credential(&self, credential: Credential) {
        *self.credential() = credential;
    }

    /// The endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Puts `body` at `key` with `metadata`, in one request (no multipart), with the content
    /// type `application/octet-stream`. A put replaces what the key held.
    ///
    /// # Errors
    ///
    /// [`StoreError::TooLarge`] for a body over [`MAX_PUT_BYTES`] and [`StoreError::Invalid`]
    /// for a bad key or metadata, both before any request; otherwise the store's refusal.
    pub fn put(&self, key: &str, body: Vec<u8>, metadata: &Metadata) -> Result<(), StoreError> {
        let headers = put_headers(key, body.len(), metadata)?;
        let path = self.endpoint.object_path(key);
        let response = self.send(Method::Put, &path, &[], headers, &["content-type"], body)?;
        put_answer(&response)
    }

    /// Deletes `key`. A key that does not exist is success (S3 answers 204, some stores 404
    /// `NoSuchKey`); a 404 for a missing bucket is [`StoreError::NotFound`].
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a bad key before any request; otherwise the store's refusal.
    pub fn delete(&self, key: &str) -> Result<(), StoreError> {
        check_key(key)?;
        let path = self.endpoint.object_path(key);
        let response = self.send(Method::Delete, &path, &[], Vec::new(), &[], Vec::new())?;
        delete_answer(&response)
    }

    /// Reads `len` bytes of `key` from `start` (fewer when the object ends first), with the
    /// object's metadata and length. `None` when the key does not exist (404 `NoSuchKey`); a 404
    /// for a missing bucket is [`StoreError::NotFound`].
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a bad key, a `len` of 0 or above [`MAX_GET_BYTES`], or a
    /// range that ends past `u64::MAX`, before any request; [`StoreError::OutOfRange`] when
    /// `start` is at or past the object's end; [`StoreError::Malformed`] when the answer is not
    /// the range asked for; otherwise the store's refusal.
    pub fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Option<Object>, StoreError> {
        check_key(key)?;
        let headers = vec![("range".to_owned(), range_header(start, len)?)];
        let path = self.endpoint.object_path(key);
        let response = self.send(Method::Get, &path, &[], headers, &["range"], Vec::new())?;
        get_answer(response, start)
    }

    /// Lists at most `max_keys` keys under `prefix` in ascending order, after `start_after`
    /// when it is given (S3 ListObjectsV2). The next page starts after the last key of this one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a `max_keys` of 0 or above [`MAX_LIST_KEYS`] before any
    /// request; [`StoreError::Malformed`] for an answer the scanner refuses; otherwise the
    /// store's refusal.
    pub fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Page, StoreError> {
        let query = list_query(prefix, start_after, max_keys)?;
        let path = self.endpoint.bucket_path();
        let response = self.send(Method::Get, &path, &query, Vec::new(), &[], Vec::new())?;
        list_answer(&response, prefix, start_after, max_keys)
    }

    /// Signs and sends one request.
    fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        mut headers: Vec<(String, String)>,
        sign_also: &[&str],
        body: Vec<u8>,
    ) -> Result<HttpResponse, StoreError> {
        let credential = self.credential().clone();
        let now = self.clock.now();
        check_live(&credential, now)?;
        let host = self.endpoint.host();
        let unsigned = Unsigned {
            method: method.as_str(),
            host: &host,
            path,
            query,
            headers: &headers,
            sign_also,
            body: &body,
        };
        let scope = Scope {
            region: self.endpoint.region(),
            service: "s3",
        };
        let signed = sign(&unsigned, &credential, &scope, now);
        let url = request_url(&self.endpoint, path, query);
        headers.push(("host".to_owned(), host));
        headers.extend(signed.headers);
        let request = HttpRequest {
            method,
            url,
            headers,
            body,
        };
        self.transport.send(request).map_err(transport)
    }
}

// The checks and the reading of answers are plain functions, not methods of the generic
// `Store`: each is then one function however many transports call it.

/// Refuses a credential expired at `now`, before anything is signed or sent.
fn check_live(credential: &Credential, now: SystemTime) -> Result<(), StoreError> {
    match credential.is_expired(now) {
        true => Err(StoreError::Expired {
            code: String::new(),
        }),
        false => Ok(()),
    }
}

/// The URL of `path` on `endpoint`, with `query` canonically encoded when there is one.
fn request_url(endpoint: &Endpoint, path: &str, query: &[(String, String)]) -> String {
    let query_text = canonical_query(query);
    match query_text.is_empty() {
        true => endpoint.url(path),
        false => format!("{}?{query_text}", endpoint.url(path)),
    }
}

/// The headers of a put of `len` bytes at `key` with `metadata`, after the checks made before
/// any request.
pub(crate) fn put_headers(
    key: &str,
    len: usize,
    metadata: &Metadata,
) -> Result<Vec<(String, String)>, StoreError> {
    if len > MAX_PUT_BYTES {
        return Err(StoreError::TooLarge {
            len,
            max: MAX_PUT_BYTES,
        });
    }
    check_key(key)?;
    let mut headers = vec![(
        "content-type".to_owned(),
        "application/octet-stream".to_owned(),
    )];
    for (name, value) in metadata {
        check_metadata(name, value)?;
        headers.push((format!("{META}{name}"), value.clone()));
    }
    Ok(headers)
}

/// What the answer to a put says.
fn put_answer(response: &HttpResponse) -> Result<(), StoreError> {
    match response.status {
        200..=299 => Ok(()),
        status => Err(refusal(status, &response.body)),
    }
}

/// What the answer to a delete says.
fn delete_answer(response: &HttpResponse) -> Result<(), StoreError> {
    match response.status {
        200..=299 => Ok(()),
        404 if missing_key(&response.body) => Ok(()),
        status => Err(refusal(status, &response.body)),
    }
}

/// The `range` header of a read of `len` bytes from `start`.
fn range_header(start: u64, len: u64) -> Result<String, StoreError> {
    let last = match (1..=MAX_GET_BYTES).contains(&len) {
        true => start.checked_add(len - 1),
        false => None,
    }
    .ok_or(StoreError::Invalid(
        "a range is 1 to MAX_GET_BYTES bytes and ends at most at u64::MAX",
    ))?;
    Ok(format!("bytes={start}-{last}"))
}

/// What the answer to a read from `start` says.
fn get_answer(response: HttpResponse, start: u64) -> Result<Option<Object>, StoreError> {
    match response.status {
        206 => ranged(response, start).map(Some),
        404 if missing_key(&response.body) => Ok(None),
        416 => Err(StoreError::OutOfRange),
        status => Err(refusal(status, &response.body)),
    }
}

/// The ListObjectsV2 query of a listing, after the check made before any request.
fn list_query(
    prefix: &str,
    start_after: Option<&str>,
    max_keys: u32,
) -> Result<Vec<(String, String)>, StoreError> {
    if !(1..=MAX_LIST_KEYS).contains(&max_keys) {
        return Err(StoreError::Invalid("max_keys is 1 to MAX_LIST_KEYS"));
    }
    let mut query = vec![
        ("list-type".to_owned(), "2".to_owned()),
        ("max-keys".to_owned(), max_keys.to_string()),
        ("prefix".to_owned(), prefix.to_owned()),
    ];
    if let Some(after) = start_after {
        query.push(("start-after".to_owned(), after.to_owned()));
    }
    Ok(query)
}

/// What the answer to a listing says.
fn list_answer(
    response: &HttpResponse,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: u32,
) -> Result<Page, StoreError> {
    match response.status {
        200 => parse_page(&response.body, prefix, start_after, max_keys),
        status => Err(refusal(status, &response.body)),
    }
}

/// The object a 206 answer to a read from `start` holds: its `content-range` must be
/// `bytes start-last/total` with as many bytes as it says.
fn ranged(response: HttpResponse, start: u64) -> Result<Object, StoreError> {
    let (first, last, total_len) = response
        .header("content-range")
        .and_then(content_range)
        .ok_or(StoreError::Malformed(
            "a ranged answer has no usable content-range",
        ))?;
    let len = u64::try_from(response.body.len()).unwrap_or(u64::MAX);
    if first != start
        || last < first
        || (last - first).checked_add(1) != Some(len)
        || last >= total_len
    {
        return Err(StoreError::Malformed(
            "a ranged answer is not the range asked for",
        ));
    }
    let metadata = response
        .headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.strip_prefix(META)?;
            Some((name.to_owned(), value.clone()))
        })
        .collect();
    Ok(Object {
        bytes: response.body,
        metadata,
        total_len,
    })
}

/// The first byte, the last byte and the total of `bytes first-last/total`.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    Some((first.parse().ok()?, last.parse().ok()?, total.parse().ok()?))
}

/// Refuses a key the client does not send: empty or longer than [`MAX_KEY_LEN`].
pub(crate) fn check_key(key: &str) -> Result<(), StoreError> {
    match key.is_empty() || key.len() > MAX_KEY_LEN {
        true => Err(StoreError::Invalid("a key is 1 to MAX_KEY_LEN bytes")),
        false => Ok(()),
    }
}

/// Refuses metadata the client does not send: a name that is not 1 to
/// [`MAX_METADATA_NAME_LEN`] bytes of `[a-z0-9-]`, or a value that is not 1 to
/// [`MAX_METADATA_VALUE_LEN`] bytes of visible ASCII (no space: a signer would fold it).
fn check_metadata(name: &str, value: &str) -> Result<(), StoreError> {
    let name_ok = (1..=MAX_METADATA_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let value_ok = (1..=MAX_METADATA_VALUE_LEN).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_graphic());
    match name_ok && value_ok {
        true => Ok(()),
        false => Err(StoreError::Invalid(
            "a metadata name is [a-z0-9-] and a value visible ASCII, within their lengths",
        )),
    }
}
