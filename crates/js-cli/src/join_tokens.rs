//! `lease join-token mint`: asks the server for a one-time join token and puts it where the
//! caller said, never on a terminal.

use std::fs;
use std::io::Write;
use std::path::Path;

use js_client::Client;
use sha2::{Digest, Sha256};

use crate::api::Routes;
use crate::cli::MintArgs;
use crate::failure::{Failure, exit};
use crate::render::Render;

/// The mode of a token file.
const FILE_MODE: u32 = 0o600;

/// `lease join-token mint`: refuses a destination that cannot take the token before any
/// request is sent, so no token is minted to be thrown away; mints; writes the token to the
/// file or to standard output; and prints the sha256 and the expiry to `err`.
pub(crate) fn mint(
    client: &Client,
    render: Render,
    args: &MintArgs,
    stdout_is_terminal: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    check_destination(args.out.as_deref(), stdout_is_terminal)?;
    let minted = client.mint_join_token(&args.body())?;
    let line = format!("{}\n", minted.token);
    deliver(args.out.as_deref(), &line, out).map_err(|(dest, why)| Failure::Undelivered {
        dest,
        why,
        expires_at_ms: minted.expires_at_ms,
    })?;
    let sha256 = hex::encode(Sha256::digest(minted.token.as_bytes()));
    let _ = err.write_all(render.minted(&sha256, minted.expires_at_ms).as_bytes());
    Ok(exit::OK)
}

/// Whether the token has somewhere to go: a file that is not there yet in a directory that
/// is, or a standard output that is not a terminal.
fn check_destination(out: Option<&Path>, stdout_is_terminal: bool) -> Result<(), Failure> {
    match out {
        None if stdout_is_terminal => Err(Failure::Usage(
            "standard output is a terminal, and a token is never printed to one: pipe it \
             (| js-agent enroll ...) or give --out PATH"
                .to_owned(),
        )),
        None => Ok(()),
        Some(path) => free(path),
    }
}

/// Whether `path` can be made: nothing is there (a link, even to nothing, is something) and
/// its directory is.
fn free(path: &Path) -> Result<(), Failure> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let problems = [
        (
            fs::symlink_metadata(path).is_ok(),
            "exists; a token is never written over a file",
        ),
        (!dir.is_dir(), "is in a directory that does not exist"),
    ];
    match problems.iter().find(|(bad, _)| *bad) {
        Some((_, why)) => Err(Failure::flag("--out", format!("{} {why}", path.display()))),
        None => Ok(()),
    }
}

/// Writes `line` to the new file `path` (mode 0600, made whole and never over a file), or
/// to `out`. The error is where it should have gone and the system's reason.
fn deliver(path: Option<&Path>, line: &str, out: &mut dyn Write) -> Result<(), (String, String)> {
    match path {
        Some(path) => js_client::create_new(path, line.as_bytes(), FILE_MODE)
            .map_err(|why| (path.display().to_string(), why.to_string())),
        None => out
            .write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|why| ("standard output".to_owned(), why.to_string())),
    }
}
