//! The `js-bundle` command line.
//!
//! - `pack` makes an OCI image-layout directory holding a darwin bundle from directories
//!   (one tree each) and prints the manifest digest. The output is reproducible: the same
//!   trees and flags give the same bytes, so the same digests, wherever the trees are.
//!   Whatever the format refuses (a name, a mode, a symlink) stops the command before
//!   anything is written.
//! - `inspect` prints the manifest and the config of a bundle in a layout.
//! - `verify` reads a bundle through [`js_oci::Layout`], which checks the manifest
//!   against its digest and the artifact type, media types and limits, and every blob
//!   against its length and digest; parses the config under the format's rules; and
//!   extracts every layer into a scratch directory with the unprivileged extractor, which
//!   checks the layer's diff id and the tree rules.
//!
//! There is no registry push here, and no environment variable is read.

#![allow(dead_code)]

mod bundle;
mod cli;
mod error;
mod inspect;
mod layout;
mod pack;
mod verify;

use std::io::Write;
use std::process::ExitCode;

use clap::Parser as _;

pub use cli::{Cli, Command, EnvFlag, LayoutArgs, PackArgs, Pair, VerifyArgs};
pub use error::Error;

/// Runs the command line: parses the arguments, writes results to standard output and a
/// failure to standard error.
#[must_use]
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli.command, &mut std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-bundle: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one command, writing its results to `out`.
///
/// # Errors
/// The command's failure; nothing is written to the layout in that case when it is
/// `pack` refusing a tree.
pub fn execute(command: Command, out: &mut dyn Write) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    runtime.block_on(dispatch(command, out))
}

async fn dispatch(command: Command, out: &mut dyn Write) -> Result<(), Error> {
    match command {
        Command::Pack(args) => pack::pack(&args, out),
        Command::Inspect(args) => inspect::inspect(&args, out).await,
        Command::Verify(args) => verify::verify(&args, out).await,
    }
}
