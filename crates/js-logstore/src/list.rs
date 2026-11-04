//! A strict scanner of the answer to S3's ListObjectsV2: the few elements the client uses
//! (`IsTruncated`, and `Key`, `Size` and `LastModified` of each `Contents`). Everything else in
//! the answer is ignored; an element the client uses that is missing or malformed makes the
//! whole answer malformed.

use std::time::SystemTime;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::StoreError;

/// One object of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The object's key.
    pub key: String,
    /// The object's length in bytes.
    pub size: u64,
    /// When the object was last written, as the store says.
    pub last_modified: SystemTime,
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The objects, in ascending key order.
    pub entries: Vec<Entry>,
    /// Whether more objects follow the last one: the next page starts after its key.
    pub truncated: bool,
}

/// The page `body` holds. Its keys must be in strictly ascending order, after `start_after`,
/// under `prefix`, and at most `max_keys` of them; a truncated page must hold at least one.
pub(crate) fn parse_page(
    body: &[u8],
    prefix: &str,
    start_after: Option<&str>,
    max_keys: u32,
) -> Result<Page, StoreError> {
    let text = std::str::from_utf8(body).map_err(|_| malformed("the listing is not UTF-8"))?;
    let root = root(text).ok_or(malformed("no ListBucketResult"))?;
    let truncated = match element(root, "IsTruncated") {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(malformed("no IsTruncated of true or false")),
    };
    let mut entries: Vec<Entry> = Vec::new();
    let mut rest = root;
    while let Some((_, after)) = rest.split_once("<Contents>") {
        let (contents, tail) = after
            .split_once("</Contents>")
            .ok_or(malformed("a Contents is not closed"))?;
        let entry = parse_entry(contents)?;
        let floor = entries.last().map(|last| last.key.as_str()).or(start_after);
        let in_order = floor.is_none_or(|floor| entry.key.as_str() > floor);
        if !in_order || !entry.key.starts_with(prefix) {
            return Err(malformed("a key is out of order or outside the prefix"));
        }
        entries.push(entry);
        rest = tail;
    }
    let too_many = u64::try_from(entries.len()).unwrap_or(u64::MAX) > u64::from(max_keys);
    if too_many || (truncated && entries.is_empty()) {
        return Err(malformed(
            "the page is longer than asked, or truncated and empty",
        ));
    }
    Ok(Page { entries, truncated })
}

/// The entry a `Contents` element's inside describes.
fn parse_entry(contents: &str) -> Result<Entry, StoreError> {
    let key = element(contents, "Key")
        .and_then(unescape)
        .ok_or(malformed("a Key is missing or badly escaped"))?;
    let size = element(contents, "Size")
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or(malformed("a Size is missing or not a number"))?;
    let last_modified = element(contents, "LastModified")
        .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
        .map(SystemTime::from)
        .ok_or(malformed("a LastModified is missing or not RFC 3339"))?;
    Ok(Entry {
        key,
        size,
        last_modified,
    })
}

/// The inside of the `ListBucketResult` element. Its open tag may carry attributes (S3 names
/// its XML namespace there); a self-closing one, or another element whose name merely begins
/// the same, is not it.
fn root(text: &str) -> Option<&str> {
    let (_, after) = text.split_once("<ListBucketResult")?;
    let (attributes, rest) = after.split_once('>')?;
    let (inside, _) = rest.split_once("</ListBucketResult>")?;
    let open_tag = attributes.is_empty() || attributes.starts_with(char::is_whitespace);
    (open_tag && !attributes.ends_with('/')).then_some(inside)
}

/// The text between the first `<name>` and the `</name>` after it.
fn element<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let (_, after) = text.split_once(open.as_str())?;
    after.split_once(close.as_str()).map(|(inside, _)| inside)
}

/// `text` with XML's character references replaced: the five named ones and `&#N;`/`&#xN;`.
/// Any other `&` is `None`, and so is a `<`.
fn unescape(text: &str) -> Option<String> {
    (!text.contains('<')).then_some(())?;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((plain, after)) = rest.split_once('&') {
        out.push_str(plain);
        let (reference, tail) = after.split_once(';')?;
        out.push(character(reference)?);
        rest = tail;
    }
    out.push_str(rest);
    Some(out)
}

/// The character the reference `name` (between `&` and `;`) stands for.
fn character(name: &str) -> Option<char> {
    let code = match name {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => name.strip_prefix('#')?,
    };
    let value = match code.strip_prefix('x') {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => code.parse().ok()?,
    };
    char::from_u32(value)
}

/// [`StoreError::Malformed`] with `why`.
fn malformed(why: &'static str) -> StoreError {
    StoreError::Malformed(why)
}
