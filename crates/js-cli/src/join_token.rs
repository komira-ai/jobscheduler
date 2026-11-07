//! `join-token mint` and `join-token revoke`: the one-time token that admits a launched
//! machine's agent, and its withdrawal.
//!
//! A token printed on stdout is the caller's to guard; `--out` writes it where it is
//! needed instead, in a file `js` creates new with mode 0600 (the agent's `--token-file`
//! takes exactly that), so the token never passes through a terminal, a shell variable or
//! a process list. With `--out` the token is not printed at all.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use js_client::Client;

use crate::api::Routes;
use crate::cli::{InstanceArgs, JoinTokenAction, MintArgs};
use crate::failure::{Failure, exit};
use crate::render::Render;

/// Runs a `join-token` command.
pub(crate) fn run(
    client: &Client,
    render: Render,
    action: JoinTokenAction,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    match action {
        JoinTokenAction::Mint(args) => mint(client, render, &args, out),
        JoinTokenAction::Revoke(args) => revoke(client, render, &args, out),
    }
}

/// A new file for the token, made before the request so that a path already taken is
/// refused while there is still nothing to lose.
struct Reserved<'a> {
    path: &'a Path,
    file: File,
}

impl<'a> Reserved<'a> {
    /// Creates `path`, which must not exist, owner-only.
    fn create(path: &'a Path) -> Result<Self, Failure> {
        let made = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
        let file = unwritable(path, made)?;
        Ok(Self { path, file })
    }

    /// Writes the token and a newline.
    fn fill(&self, token: &str) -> Result<(), Failure> {
        let mut file = &self.file;
        let written = file.write_all(format!("{token}\n").as_bytes());
        let synced = written.and_then(|()| file.sync_all());
        unwritable(self.path, synced)
    }

    /// Removes the file of a mint that made no token (or whose token was not written).
    fn discard(&self) {
        let _ = fs::remove_file(self.path);
    }
}

/// `done`, or the usage failure of a token file that cannot be made or written.
fn unwritable<T>(path: &Path, done: std::io::Result<T>) -> Result<T, Failure> {
    done.map_err(|why| Failure::flag("--out", format!("{}: {why}", path.display())))
}

/// `join-token mint`: asks for a token, then prints it or writes it to `--out`.
fn mint(
    client: &Client,
    render: Render,
    args: &MintArgs,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let reserved = args.out.as_deref().map(Reserved::create).transpose()?;
    let made = client.mint_join_token(&args.body()).and_then(|minted| {
        let filled = reserved.as_ref().map_or(Ok(()), |r| r.fill(&minted.token));
        filled.map(|()| minted)
    });
    if let (Err(_), Some(reserved)) = (&made, &reserved) {
        reserved.discard();
    }
    let minted = made?;
    let shown = match &args.out {
        Some(path) => render.token_written(&args.instance, path, minted.expires_at_ms),
        None => render.token(&minted),
    };
    let _ = out.write_all(shown.as_bytes());
    Ok(exit::OK)
}

/// `join-token revoke`: drops every token of the instance, used or not.
fn revoke(
    client: &Client,
    render: Render,
    args: &InstanceArgs,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    client.revoke_join_tokens(&args.body())?;
    let _ = out.write_all(render.revoked(args).as_bytes());
    Ok(exit::OK)
}
