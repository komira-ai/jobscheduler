//! `pack`: stub.

use std::io::Write;

use crate::cli::PackArgs;
use crate::error::Error;

pub(crate) fn pack(_args: &PackArgs, _out: &mut dyn Write) -> Result<(), Error> {
    todo!("pack")
}
