//! `verify`: stub.

use std::io::Write;

use crate::cli::VerifyArgs;
use crate::error::Error;

pub(crate) async fn verify(_args: &VerifyArgs, _out: &mut dyn Write) -> Result<(), Error> {
    todo!("verify")
}
