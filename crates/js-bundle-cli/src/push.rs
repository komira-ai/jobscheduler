//! `push`: a bundle in a layout, put into a registry under a tag.
//!
//! The bundle is read as `inspect` reads it ([`load`]: the manifest against its digest and
//! the bundle kind, the config under the format's rules), then published by
//! [`js_oci::Repository::publish`]: the tag's manifest is looked up first, so a tag that
//! names this manifest already is left as it is (`unchanged`), one that names another is
//! refused and never moved, and otherwise every blob is uploaded and the manifest put under
//! the tag (`pushed`). Only the repository `--to` names is on the client's allow-list.
//!
//! The registry's token realm is asked for a push token as `--user` with the password or
//! token in `--password-file` (the realm on the registry's own host, over https, alone), or
//! anonymously without them. No environment variable is read.

use std::io::Write;

use js_oci::{Allow, Client, Credentials, Kind, Location, Policy, Published, Tag};

use crate::bundle::load;
use crate::cli::PushArgs;
use crate::error::{Error, at};

pub(crate) async fn push(args: &PushArgs, out: &mut dyn Write) -> Result<(), Error> {
    let (name, tag) = args
        .to
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
        .ok_or_else(|| Error::Target(args.to.clone()))?;
    let tag = Tag::parse(tag)?;
    let location = Location::parse(name)?;
    let mut entry = Allow::parse(name)?;
    if args.plain_http {
        entry = entry.loopback().plain_http_loopback();
    }
    if let Some(credentials) = credentials(args)? {
        entry = entry.with_credentials(credentials);
    }
    let loaded = load(&args.source).await?;
    let client = Client::new(Policy::new(vec![entry]));
    let repository = client.repository(location)?;
    let digest = loaded.manifest.digest();
    let done = repository
        .publish(&loaded.layout, digest, &Kind::bundle(), &tag)
        .await?;
    let word = match done {
        Published::Pushed => "pushed",
        Published::AlreadyThere => "unchanged",
    };
    writeln!(out, "{word} {name}:{tag}@{digest}")?;
    Ok(())
}

/// The credentials `--user` and `--password-file` give, if they are given: the file's text
/// with one trailing line end dropped.
fn credentials(args: &PushArgs) -> Result<Option<Credentials>, Error> {
    let (Some(user), Some(path)) = (&args.user, &args.password_file) else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).map_err(at(path))?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Password {
        path: path.display().to_string(),
    })?;
    let password = text.strip_suffix('\n').unwrap_or(&text);
    let password = password.strip_suffix('\r').unwrap_or(password);
    Ok(Some(Credentials::new(user, password)?))
}
