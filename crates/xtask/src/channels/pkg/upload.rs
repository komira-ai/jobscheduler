//! `channels pkg upload`: the signed Mac package and its `SHA256SUMS` into
//! `pkg/jobscheduler/<version>/`.
//!
//! Before anything is put, the `SHA256SUMS` in `--dir` must list exactly the package, at the
//! digest of the file. The package is put first and `SHA256SUMS` after it, so a reader that finds
//! the sums finds the package they list. Each file is put with one `aws s3api put-object ...
//! --if-none-match '*'`, a single-part upload, the only kind the publishing role may make. A file
//! that exists answers `412`; the stored file is then read back, and the answer is a success only
//! if its digest is the local file's: a re-run of the job passes, and a different file at a
//! version's key is never written over (the role cannot, and this refuses to pretend).

use std::path::Path;

use super::{IMMUTABLE, Request, check_request, file_name, version_prefix};
use crate::channels::Error;
use crate::channels::apt::{Failure, classify, strings, why};
use crate::channels::aws::Aws;
use crate::channels::sums::{SUMS, parse};
use crate::fetch::Sha256;

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(Error::spawn(format!("read {}", path.display())))
}

/// The `Content-Type` a file of `name` is served as.
fn content_type(name: &str) -> &'static str {
    if name == SUMS {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Puts `path` at `key` once. A `412` passes only if the stored file's digest is `sha256`.
fn put_once(
    aws: &dyn Aws,
    r: &Request,
    key: &str,
    name: &str,
    path: &Path,
    sha256: &Sha256,
) -> Result<String, Error> {
    let put = aws.run(&strings(&[
        "s3api",
        "put-object",
        "--bucket",
        r.bucket,
        "--key",
        key,
        "--body",
        &path.display().to_string(),
        "--if-none-match",
        "*",
        "--content-type",
        content_type(name),
        "--cache-control",
        IMMUTABLE,
    ]))?;
    if put.ok {
        return Ok(format!("{key}: written"));
    }
    if classify(&put.stderr) != Failure::Precondition {
        return Err(Error::Failed(format!("put {key}: {}", why(&put))));
    }
    let existing = r.work.join("existing");
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        key,
        &existing.display().to_string(),
    ]))?;
    if !got.ok {
        return Err(Error::Failed(format!(
            "{key} exists, and reading it back failed: {}",
            why(&got)
        )));
    }
    let stored = Sha256::of(&read(&existing)?);
    if stored == *sha256 {
        return Ok(format!("{key}: already there with the same bytes"));
    }
    Err(Error::Failed(format!(
        "{key} exists with other bytes (sha256 {stored}, this release's file is {sha256}): a file of a version is never replaced"
    )))
}

/// The digests of the package `name` and of the `SHA256SUMS` beside it, after the sums are held
/// to list exactly the package at its digest.
fn checked(r: &Request, name: &str) -> Result<(Sha256, Sha256), Error> {
    let digest = Sha256::of(&read(&r.dir.join(name))?);
    let sums_path = r.dir.join(SUMS);
    let sums_bytes = read(&sums_path)?;
    let listed = parse(&String::from_utf8_lossy(&sums_bytes)).map_err(|problems| {
        Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", sums_path.display()))
                .collect(),
        )
    })?;
    let [line] = listed.as_slice() else {
        let names: Vec<&str> = listed.iter().map(|l| l.name.as_str()).collect();
        return Err(Error::Refused(vec![format!(
            "{SUMS} must list exactly {name}; it lists {}",
            names.join(", ")
        )]));
    };
    if line.name != name {
        return Err(Error::Refused(vec![format!(
            "{SUMS} lists {}, not {name}",
            line.name
        )]));
    }
    if line.sha256 != digest {
        return Err(Error::Refused(vec![format!(
            "{name} is {digest}, but {SUMS} lists {}",
            line.sha256
        )]));
    }
    Ok((digest, Sha256::of(&sums_bytes)))
}

/// The `upload` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a package missing or not the one `SHA256SUMS` lists exactly, a refusal
/// by AWS, or a file of the version that exists with other bytes.
pub fn upload(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    check_request(r)?;
    let name = file_name(r.version);
    let (digest, sums_digest) = checked(r, &name)?;
    std::fs::create_dir_all(r.work)
        .map_err(Error::spawn(format!("create {}", r.work.display())))?;
    let prefix = version_prefix(r.version);
    let package = put_once(
        aws,
        r,
        &format!("{prefix}{name}"),
        &name,
        &r.dir.join(&name),
        &digest,
    )?;
    let sums = put_once(
        aws,
        r,
        &format!("{prefix}{SUMS}"),
        SUMS,
        &r.dir.join(SUMS),
        &sums_digest,
    )?;
    Ok(vec![package, sums])
}
