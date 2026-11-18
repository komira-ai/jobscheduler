//! `channels descriptor write`: jobscheduler's entry in the repository's descriptor,
//! `hostkeeper-repository.json` at the root of the bucket of `packages.komira.ai`.
//!
//! A hostkeeper coordinator finds a `macos-pkg` installer through that descriptor: its
//! `formats.macos-pkg.packages` maps a package name to the directory below the format's `path`
//! that holds the package's versions, and a name with no entry is looked for in `pkg/<name>/`.
//! The release of js-agent puts its package in `pkg/jobscheduler/<version>/` ([`super::pkg`]), so
//! the descriptor must say that `js-agent` lives in `jobscheduler`.
//!
//! The descriptor is shared by the organisation's products and created by hostkeeper's release;
//! this verb only **adds** to it ([`merge`]) and writes nothing else:
//!
//! - `formats.macos-pkg.path` is `pkg` (set when absent, refused when it is another value);
//! - `formats.macos-pkg.platforms` holds `macos-arm64` (united with what is there);
//! - `formats.macos-pkg.packages.js-agent` is `{"dir": "jobscheduler"}` (an entry with another
//!   directory is refused, naming both).
//!
//! Every other key (`apt`, `rpm`, `key`, `channels`, other products' packages, keys this build
//! does not know) is kept as the JSON value it was. A descriptor that is absent is refused:
//! hostkeeper's release creates it, with the key and the channels no product here writes. So is
//! a version newer than this build reads, and a part that is not of its form.
//!
//! [`write`] reads the descriptor with its ETag, merges, and puts the result with
//! `If-Match` on that ETag, a single-part upload; a `412` means another release wrote first, and
//! it reads and decides again, up to `--attempts` times. When the merge changes nothing it writes
//! nothing and invalidates nothing. After a write it invalidates the path in the CDN when `--distribution-id` is given.
//!
//! - `channels descriptor write --bucket <b> --work <dir> [--distribution-id <id>] [--attempts
//!   <n>] [--aws <program>]`. Job `pkg-upload` of `release.yml`, after the upload: the cloud
//!   role, no key.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Map, Value, json};

use super::apt::{Failure, classify, strings, why};
use super::aws::{Aws, RealAws};
use super::{Error, is_bucket, is_distribution_id};

/// The key of the descriptor in the bucket.
pub const FILE: &str = "hostkeeper-repository.json";

/// The newest descriptor version this build reads.
pub const VERSION: u64 = 1;

/// How often the descriptor is read and decided again after a concurrent write, by default.
pub const ATTEMPTS: u32 = 3;

/// The cache header of the descriptor: it changes under its URL.
pub const CACHE: &str = "public, max-age=60";

/// The platform of the installer package.
pub const PLATFORM: &str = "macos-arm64";

/// The package name the coordinator asks for.
pub const PACKAGE: &str = "js-agent";

/// The directory of the `macos-pkg` format below the bucket's root.
pub const PKG_PATH: &str = "pkg";

/// The directory of this repository's releases below [`PKG_PATH`].
pub const DIR: &str = "jobscheduler";

/// The `channels descriptor` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// Map `js-agent` to `pkg/jobscheduler` in the descriptor in the bucket, with `If-Match`.
    Write {
        /// The repository's bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate the descriptor in; empty for none.
        #[arg(long, default_value = "")]
        distribution_id: String,
        /// A directory for the files the verb makes.
        #[arg(long)]
        work: PathBuf,
        /// How often the descriptor is read and decided again after a concurrent write.
        #[arg(long, default_value_t = ATTEMPTS)]
        attempts: u32,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
}

/// What `write` is asked.
pub struct Request<'a> {
    /// The repository's bucket.
    pub bucket: &'a str,
    /// The CDN distribution to invalidate the descriptor in; empty for none.
    pub distribution_id: &'a str,
    /// A directory for the files the verb makes.
    pub work: &'a Path,
    /// How often the descriptor is read and decided again after a concurrent write.
    pub attempts: u32,
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Write {
            bucket,
            distribution_id,
            work,
            attempts,
            aws,
        } => write(
            &RealAws(aws),
            &Request {
                bucket: &bucket,
                distribution_id: &distribution_id,
                work: &work,
                attempts,
            },
        ),
    }
}

/// `object[key]` as a mutable object, made when it is not there; an error when it is there and is
/// no object. `path` is the dotted path of `object`, for the message.
fn object_at<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a mut Map<String, Value>, Error> {
    object
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Failed(format!("the descriptor's {path}{key} is not an object")))
}

/// `existing` (the descriptor in the bucket) with jobscheduler's facts merged in; see the module
/// text. Every key it has no word about is kept as it is.
///
/// # Errors
/// A descriptor of a version newer than this build reads, a part that is not of its form, a path
/// that is not `pkg`, or a `js-agent` mapped to another directory.
pub fn merge(existing: Value) -> Result<Value, Error> {
    let mut merged = existing;
    let root = merged.as_object_mut().ok_or_else(|| {
        Error::Failed("the descriptor in the bucket is not a JSON object".to_owned())
    })?;
    if let Some(version) = root.get("version") {
        match version.as_u64() {
            Some(n) if n > VERSION => {
                return Err(Error::Failed(format!(
                    "the descriptor in the bucket is version {n}, newer than this build reads ({VERSION})"
                )));
            }
            Some(_) => {}
            None => {
                return Err(Error::Failed(format!(
                    "the descriptor's version is {version}, not a number"
                )));
            }
        }
    }
    let formats = object_at(root, "formats", "")?;
    let mac = object_at(formats, "macos-pkg", "formats.")?;
    match mac.get("path") {
        None => {
            mac.insert("path".to_owned(), json!(PKG_PATH));
        }
        Some(Value::String(path)) if path == PKG_PATH => {}
        Some(other) => {
            return Err(Error::Failed(format!(
                "the descriptor's formats.macos-pkg.path is {other}, and the release writes its package to {PKG_PATH}"
            )));
        }
    }
    let platforms = mac
        .entry("platforms")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            Error::Failed("the descriptor's formats.macos-pkg.platforms is not a list".to_owned())
        })?;
    if !platforms.contains(&json!(PLATFORM)) {
        platforms.push(json!(PLATFORM));
    }
    let packages = object_at(mac, "packages", "formats.macos-pkg.")?;
    match packages.get(PACKAGE) {
        None => {
            packages.insert(PACKAGE.to_owned(), json!({ "dir": DIR }));
        }
        Some(there) if there["dir"] == json!(DIR) => {}
        Some(there) => {
            return Err(Error::Failed(format!(
                "the descriptor maps {PACKAGE} to the directory {}, and this release maps it to {DIR}",
                there["dir"]
            )));
        }
    }
    Ok(merged)
}

/// Reads the descriptor of the bucket into `dest`: its ETag, or none when it is not there
/// (`NoSuchKey`, or `AccessDenied`: the publish role may not list the bucket, so S3 answers a
/// missing key with that).
fn fetch(aws: &dyn Aws, r: &Request, dest: &Path) -> Result<Option<String>, Error> {
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        FILE,
        &dest.display().to_string(),
    ]))?;
    if !got.ok {
        return match classify(&got.stderr) {
            Failure::NotFound | Failure::Denied => Ok(None),
            _ => Err(Error::Failed(format!("read {FILE}: {}", why(&got)))),
        };
    }
    super::apt::read::etag_of(&got.stdout)
        .map(Some)
        .ok_or_else(|| Error::Failed(format!("read {FILE}: aws printed no ETag")))
}

/// Invalidates `/<FILE>` in the CDN, when a distribution is given: the line to print.
fn invalidate(aws: &dyn Aws, r: &Request) -> Result<Option<String>, Error> {
    if r.distribution_id.is_empty() {
        return Ok(None);
    }
    let path = format!("/{FILE}");
    let made = aws.run(&strings(&[
        "cloudfront",
        "create-invalidation",
        "--distribution-id",
        r.distribution_id,
        "--paths",
        &path,
    ]))?;
    if !made.ok {
        return Err(Error::Failed(format!("invalidate {path}: {}", why(&made))));
    }
    Ok(Some(format!("{path}: invalidated")))
}

/// The flags, held to their forms.
fn check_request(r: &Request) -> Result<(), Error> {
    if !is_bucket(r.bucket) {
        return Err(Error::Usage(format!(
            "--bucket {:?} is not a bucket name",
            r.bucket
        )));
    }
    if !r.distribution_id.is_empty() && !is_distribution_id(r.distribution_id) {
        return Err(Error::Usage(format!(
            "--distribution-id {:?} is not a distribution id",
            r.distribution_id
        )));
    }
    if r.attempts == 0 {
        return Err(Error::Usage(
            "--attempts is 0; it must be 1 or more".to_owned(),
        ));
    }
    Ok(())
}

/// The `write` verb: jobscheduler's facts merged into the descriptor of the bucket.
///
/// # Errors
/// A flag out of its form, a descriptor that is not in the bucket, is not JSON or cannot be
/// merged with, a refusal by AWS, or the object changed under every attempt.
pub fn write(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    check_request(r)?;
    std::fs::create_dir_all(r.work)
        .map_err(Error::spawn(format!("create {}", r.work.display())))?;
    let current = r.work.join("descriptor-current.json");
    let next = r.work.join("descriptor-next.json");
    for _ in 0..r.attempts {
        let Some(etag) = fetch(aws, r, &current)? else {
            return Err(Error::Failed(format!(
                "{FILE} is not in the bucket (or the role cannot read it): hostkeeper's release creates it, and this release only adds to it"
            )));
        };
        let bytes =
            std::fs::read(&current).map_err(Error::spawn(format!("read {}", current.display())))?;
        let existing = serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| Error::Failed(format!("{FILE} in the bucket is not JSON: {e}")))?;
        let merged = merge(existing.clone())?;
        if merged == existing {
            return Ok(vec![format!(
                "{FILE}: already maps {PACKAGE} to {PKG_PATH}/{DIR}; nothing written"
            )]);
        }
        std::fs::write(&next, format!("{merged:#}\n"))
            .map_err(Error::spawn(format!("write {}", next.display())))?;
        let put = aws.run(&strings(&[
            "s3api",
            "put-object",
            "--bucket",
            r.bucket,
            "--key",
            FILE,
            "--body",
            &next.display().to_string(),
            "--content-type",
            "application/json",
            "--cache-control",
            CACHE,
            "--if-match",
            &etag,
        ]))?;
        if put.ok {
            let mut said = vec![format!("{FILE}: written")];
            said.extend(invalidate(aws, r)?);
            return Ok(said);
        }
        if classify(&put.stderr) != Failure::Precondition {
            return Err(Error::Failed(format!("put {FILE}: {}", why(&put))));
        }
    }
    Err(Error::Failed(format!(
        "{FILE} was written by another release {} times in a row; run the job again",
        r.attempts
    )))
}

#[cfg(test)]
mod tests;
