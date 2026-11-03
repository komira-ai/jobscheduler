//! `--registry-credentials`: the user and password an allow-list entry sends to its
//! registry's token realm, so a task image or a bundle in a private package can be
//! pulled (GitHub's container registry issues pull tokens for one only to a user it
//! knows).
//!
//! # The flag
//!
//! `--registry-credentials HOST[:PORT]/PREFIX=PATH`, repeatable: an entry of
//! `--image-allow`, `--bundle-registry-allow` or `--bundle-registry-allow-private`,
//! spelled as it is there, and the file that holds its credentials. The command line
//! names the file only; the credentials are never on it, and no environment variable is
//! read.
//!
//! # The file
//!
//! One line, `USER:PASSWORD`: the user is everything before the first colon, the
//! password (a registry token, typically) everything after it, and one line ending
//! (`\n` or `\r\n`) is dropped. The user may not be empty; the password may not be
//! empty and neither may hold a control character, so a second line, a blank line or a
//! stray carriage return after the first is refused.
//!
//! The file must be a regular file (not a link: it is opened with `O_NOFOLLOW`) owned by
//! root, with no permission bit for group or others (`0600` or `0400`), at most
//! [`CREDENTIALS_MAX`] bytes, in a directory owned by root that group and others cannot
//! write. It is read once, when the agent starts; a change takes effect at the next start.
//!
//! # What stops the agent at start
//!
//! A flag that is not `ENTRY=PATH`; an entry that is not `HOST[:PORT]/PREFIX`, that is
//! given twice, or that no allow flag lists; a file that breaks a rule above or does not
//! hold `USER:PASSWORD`. The error names the flag, the entry or the file, and never holds
//! a byte of the file.
//!
//! # Where the credentials go
//!
//! js-oci sends them as `Authorization: Basic` to the token realm the registry's
//! challenge names, and only when that realm passed js-oci's realm checks and is the
//! registry's own host and port, over https. Never to a manifest or blob request, never
//! to a redirect's target, never over plain http, never to a realm on another host (see
//! `js_oci::Allow::with_credentials`). Repositories whose most specific entry has none are
//! pulled anonymously, as before.
//!
//! # Threat notes
//!
//! - Whoever can answer as the registry's host over TLS can read the credentials: its
//!   operator, or anyone holding a certificate for it that the host trusts. Give the
//!   agent a token that can only pull, and only the packages its jobs need.
//! - Root can read the file; nothing else on the host may. Jobs never see it: they run
//!   as other users, and the agent hands them nothing of it.
//! - The agent keeps the credentials in memory for as long as it runs. Its `Debug`
//!   output, its errors, its logs and the run records hold none of them.

use std::path::{Path, PathBuf};

use js_oci::{Allow, Credentials};

use crate::Cli;
use crate::trusted_file::{self, SECRET};

/// The most bytes a credentials file may have.
pub const CREDENTIALS_MAX: u64 = 4096;

/// Why the `--registry-credentials` flags cannot be used. No variant holds a byte of a
/// credentials file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryCredentialsError {
    /// A flag that is not `ENTRY=PATH`.
    #[error("--registry-credentials: {0:?} is not HOST[:PORT]/PREFIX=PATH")]
    Form(String),
    /// An entry that is not `HOST[:PORT]/PREFIX`; the text says why.
    #[error("--registry-credentials: {0}")]
    Entry(String),
    /// An entry given more than once.
    #[error("--registry-credentials: {0} is given more than once")]
    Twice(String),
    /// An entry that no allow flag lists.
    #[error(
        "--registry-credentials: {0} is not an entry of --image-allow, \
         --bundle-registry-allow or --bundle-registry-allow-private"
    )]
    Unlisted(String),
    /// A file that cannot be read or breaks a rule of the module; `why` says which.
    #[error("--registry-credentials: {}: {why}", path.display())]
    File {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// A file that does not hold `USER:PASSWORD`; `why` says which rule it breaks.
    #[error("--registry-credentials: {}: {why}", path.display())]
    Content {
        /// The file.
        path: PathBuf,
        /// The rule the content breaks.
        why: String,
    },
}

/// The credentials of the `--registry-credentials` flags, by allow-list entry.
///
/// `Debug` prints the entries and `Credentials(<redacted>)` for each secret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryCredentials {
    entries: Vec<(Allow, Credentials)>,
}

impl RegistryCredentials {
    /// Checks the flags and reads each file, which must be owned by `owner` (root,
    /// outside a test).
    ///
    /// # Errors
    ///
    /// See [`RegistryCredentialsError`]: the first flag that cannot be used.
    pub fn from_cli(cli: &Cli, owner: u32) -> Result<Self, RegistryCredentialsError> {
        let listed: Vec<Allow> = cli
            .image_allow
            .iter()
            .chain(&cli.bundle_registry_allow)
            .chain(&cli.bundle_registry_allow_private)
            .filter_map(|text| Allow::parse(text).ok())
            .collect();
        let mut entries: Vec<(Allow, Credentials)> = Vec::new();
        for flag in &cli.registry_credentials {
            let (text, path) = flag
                .split_once('=')
                .ok_or_else(|| RegistryCredentialsError::Form(flag.clone()))?;
            let entry = Allow::parse(text)
                .map_err(|why| RegistryCredentialsError::Entry(why.to_string()))?;
            if entries.iter().any(|(seen, _)| *seen == entry) {
                return Err(RegistryCredentialsError::Twice(text.to_owned()));
            }
            if !listed.contains(&entry) {
                return Err(RegistryCredentialsError::Unlisted(text.to_owned()));
            }
            let credentials = read(Path::new(path), owner)?;
            entries.push((entry, credentials));
        }
        Ok(Self { entries })
    }

    /// `entry` (as parsed, before [`Allow::private`] or [`Allow::loopback`]) with the
    /// credentials a flag gave it, or as it is when none did.
    #[must_use]
    pub fn attach(&self, entry: Allow) -> Allow {
        let found = self.entries.iter().find(|(listed, _)| *listed == entry);
        match found.map(|(_, credentials)| credentials.clone()) {
            Some(credentials) => entry.with_credentials(credentials),
            None => entry,
        }
    }

    /// The `--image-allow` entries of `cli`, each checked and given its credentials: the
    /// allow-list of the crun runtime's image client.
    ///
    /// # Errors
    ///
    /// An entry that is not `HOST[:PORT]/PREFIX`, naming `--image-allow`.
    pub fn image_allow(&self, cli: &Cli) -> Result<Vec<Allow>, String> {
        crate::lifecycle::image_allow(&cli.image_allow).map(|allow| self.attach_all(allow))
    }

    /// Every entry of `entries` with its credentials, in order ([`Self::attach`]).
    #[must_use]
    pub fn attach_all(&self, entries: Vec<Allow>) -> Vec<Allow> {
        entries
            .into_iter()
            .map(|entry| self.attach(entry))
            .collect()
    }
}

/// The credentials in the file at `path`, which must keep the rules of the module.
fn read(path: &Path, owner: u32) -> Result<Credentials, RegistryCredentialsError> {
    let file = |why: String| RegistryCredentialsError::File {
        path: path.to_owned(),
        why,
    };
    let content = |why: String| RegistryCredentialsError::Content {
        path: path.to_owned(),
        why,
    };
    let text = trusted_file::read(path, owner, SECRET, CREDENTIALS_MAX)
        .map_err(|why| file(why.to_string()))?;
    // One line ending is dropped, `\n` or `\r\n`; anything left (a blank line, a stray
    // carriage return) stays in the password, which refuses control characters.
    let line = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    let (user, password) = line
        .split_once(':')
        .ok_or_else(|| content("does not hold USER:PASSWORD".to_owned()))?;
    Credentials::new(user, password).map_err(|why| content(why.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser as _;
    use js_testkit::dir::TestDir;

    use super::*;

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// A private directory for credentials files.
    fn private_dir(name: &str) -> TestDir {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).expect("dir mode");
        dir
    }

    /// Writes `text` to `name` in `dir` at `mode`.
    fn secret(dir: &TestDir, name: &str, text: &[u8], mode: u32) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("mode");
        path
    }

    fn cli(extra: &[String]) -> Cli {
        let mut args = vec![
            "js-agent".to_owned(),
            "--server=https://leases.example".to_owned(),
            "--pool=default".to_owned(),
            "--image-allow=registry.example/team".to_owned(),
            "--bundle-registry-allow=bundles.example/tools/".to_owned(),
            "--bundle-registry-allow-private=registry.internal.example:5000/tools".to_owned(),
        ];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).expect("flags")
    }

    fn flag(entry: &str, path: &Path) -> String {
        format!("--registry-credentials={entry}={}", path.display())
    }

    fn entry(text: &str) -> Allow {
        Allow::parse(text).expect("entry")
    }

    fn credentials(user: &str, password: &str) -> Credentials {
        Credentials::new(user, password).expect("credentials")
    }

    /// Proves the flags give each entry of every allow flag its own file's credentials,
    /// a password with colons and a trailing line break read as the file means them,
    /// and an entry with no flag left alone. Catches credentials attached to the wrong
    /// entry, or a line break kept in the password.
    #[test]
    fn each_listed_entry_gets_the_credentials_of_its_file() {
        let dir = private_dir("registry-credentials-ok");
        let image = secret(&dir, "image", b"puller:tok:en\n", 0o600);
        let bundle = secret(&dir, "bundle", b"tools:pw\r\n", 0o400);
        let private = secret(&dir, "private", b"inside:pw", 0o600);
        let cli = cli(&[
            flag("registry.example/team", &image),
            flag("bundles.example/tools/", &bundle),
            flag("registry.internal.example:5000/tools", &private),
        ]);
        let table = RegistryCredentials::from_cli(&cli, me()).expect("credentials");
        let image_entry = entry("registry.example/team");
        let unlisted = entry("registry.example/other");
        assert_eq!(
            table.attach_all(vec![image_entry.clone(), unlisted.clone()]),
            [
                image_entry.with_credentials(credentials("puller", "tok:en")),
                unlisted,
            ]
        );
        assert_eq!(
            table.attach(entry("bundles.example/tools")),
            entry("bundles.example/tools").with_credentials(credentials("tools", "pw"))
        );
        assert_eq!(
            table.attach(entry("registry.internal.example:5000/tools")),
            entry("registry.internal.example:5000/tools")
                .with_credentials(credentials("inside", "pw"))
        );
        let none = RegistryCredentials::from_cli(&self::cli(&[]), me()).expect("no flags");
        assert_eq!(none, RegistryCredentials::default());
        assert_eq!(
            none.attach(entry("registry.example/team")),
            entry("registry.example/team")
        );
    }

    /// Proves a flag that cannot be used stops the agent, each by its own message: no
    /// `=`, an entry that is not one, the same entry twice, an entry no allow flag lists.
    /// Catches credentials for a registry the agent may not pull from, or two files for
    /// one entry with one silently winning.
    #[test]
    fn a_flag_that_names_no_listed_entry_once_is_refused() {
        let dir = private_dir("registry-credentials-flags");
        let ok = secret(&dir, "ok", b"puller:pw", 0o600);
        let once = flag("registry.example/team", &ok);
        for (flags, want) in [
            (
                vec!["--registry-credentials=registry.example/team".to_owned()],
                "--registry-credentials: \"registry.example/team\" is not HOST[:PORT]/PREFIX=PATH"
                    .to_owned(),
            ),
            (
                vec![flag("registry.example", &ok)],
                "--registry-credentials: \"registry.example\" is not a well formed registry reference"
                    .to_owned(),
            ),
            (
                vec![once.clone(), flag("registry.example/team/", &ok)],
                "--registry-credentials: registry.example/team/ is given more than once".to_owned(),
            ),
            (
                vec![flag("registry.example/other", &ok)],
                "--registry-credentials: registry.example/other is not an entry of \
                 --image-allow, --bundle-registry-allow or --bundle-registry-allow-private"
                    .to_owned(),
            ),
        ] {
            let err = RegistryCredentials::from_cli(&cli(&flags), me()).expect_err("refused");
            assert_eq!(err.to_string(), want, "{flags:?}");
        }
    }

    /// The error of a file at `path` that breaks a rule, `why` saying which.
    fn file_error(path: &Path, why: &str) -> RegistryCredentialsError {
        RegistryCredentialsError::File {
            path: path.to_owned(),
            why: why.to_owned(),
        }
    }

    /// The error of a file at `path` whose content breaks a rule.
    fn content_error(path: &Path, why: &str) -> RegistryCredentialsError {
        RegistryCredentialsError::Content {
            path: path.to_owned(),
            why: why.to_owned(),
        }
    }

    /// Proves a file is refused at start, one case each, by the exact error: a mode that
    /// lets group or others read it, a link (to a good file), one byte over the cap, not
    /// root's (here: not the expected owner's), missing, an empty path, no `USER:PASSWORD`,
    /// an empty user, a second line, a blank line and a stray carriage return after the
    /// first. No error holds a byte of the file. Catches a secret any local user could
    /// read, a link that swaps the file, an unbounded read, extra lines taken as one, and
    /// the content echoed in an error.
    #[test]
    fn loose_mode_or_symlinked_or_oversize_credentials_file_is_refused_at_start() {
        let dir = private_dir("registry-credentials-files");
        let good = secret(&dir, "good", b"puller:s3cret-pw", 0o600);
        let loose = secret(&dir, "loose", b"puller:s3cret-pw", 0o640);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&good, &link).expect("symlink");
        let mut big = b"puller:".to_vec();
        big.resize(usize::try_from(CREDENTIALS_MAX).expect("cap") + 1, b'x');
        let oversize = secret(&dir, "oversize", &big, 0o600);
        let missing = dir.path().join("missing");
        let no_colon = secret(&dir, "no-colon", b"s3cret-pw\n", 0o600);
        let empty_user = secret(&dir, "empty-user", b":s3cret-pw", 0o600);
        let two_lines = secret(&dir, "two-lines", b"puller:s3cret-pw\nmore\n", 0o600);
        let blank_line = secret(&dir, "blank-line", b"puller:s3cret-pw\n\n", 0o600);
        let stray_cr = secret(&dir, "stray-cr", b"puller:s3cret-pw\r\r\n", 0o600);
        let owner_rule =
            "not owned by the expected user, or readable or writable by group or others";
        let not_found = std::io::Error::from_raw_os_error(libc::ENOENT).to_string();
        let user_rule = "registry credentials: the user name is empty or holds a colon or a \
                         control character";
        let password_rule =
            "registry credentials: the password is empty or holds a control character";
        let empty = PathBuf::new();
        let cases = [
            (&loose, me(), file_error(&loose, owner_rule)),
            (
                &link,
                me(),
                file_error(
                    &link,
                    &std::io::Error::from_raw_os_error(libc::ELOOP).to_string(),
                ),
            ),
            (
                &oversize,
                me(),
                file_error(&oversize, "larger than the size cap"),
            ),
            (&good, me().wrapping_add(1), file_error(&good, owner_rule)),
            (&missing, me(), file_error(&missing, &not_found)),
            (&empty, me(), file_error(&empty, &not_found)),
            (
                &no_colon,
                me(),
                content_error(&no_colon, "does not hold USER:PASSWORD"),
            ),
            (&empty_user, me(), content_error(&empty_user, user_rule)),
            (&two_lines, me(), content_error(&two_lines, password_rule)),
            (&blank_line, me(), content_error(&blank_line, password_rule)),
            (&stray_cr, me(), content_error(&stray_cr, password_rule)),
        ];
        for (path, owner, want) in cases {
            let flags = [flag("registry.example/team", path)];
            let err = RegistryCredentials::from_cli(&cli(&flags), owner).expect_err("refused");
            assert_eq!(err, want, "{path:?}");
        }
        assert_eq!(
            file_error(&loose, owner_rule).to_string(),
            format!("--registry-credentials: {}: {owner_rule}", loose.display())
        );
        assert_eq!(
            content_error(&no_colon, "does not hold USER:PASSWORD").to_string(),
            format!(
                "--registry-credentials: {}: does not hold USER:PASSWORD",
                no_colon.display()
            )
        );
    }

    /// Proves the files that keep every rule are read: exactly at the cap, mode `0400`,
    /// one `\n` or `\r\n` or no line ending, and a path holding a space and `=` (the flag
    /// is split at its first `=` only). Catches a cap that is off by one, a line ending
    /// kept in the password, and a path cut at a later `=`.
    #[test]
    fn a_file_that_keeps_every_rule_is_read_at_its_boundaries() {
        let dir = private_dir("registry-credentials-bounds");
        let mut full = b"puller:".to_vec();
        let cap = usize::try_from(CREDENTIALS_MAX).expect("cap");
        full.resize(cap, b'x');
        let at_cap = secret(&dir, "at-cap", &full, 0o400);
        let password = "x".repeat(cap - "puller:".len());
        for (name, text, want) in [
            ("lf", &b"puller:pw\n"[..], "pw"),
            ("crlf", &b"puller:pw\r\n"[..], "pw"),
            ("bare", &b"puller:pw"[..], "pw"),
            ("odd name=with = signs", &b"puller:pw"[..], "pw"),
        ] {
            let path = secret(&dir, name, text, 0o600);
            let table =
                RegistryCredentials::from_cli(&cli(&[flag("registry.example/team", &path)]), me())
                    .expect(name);
            assert_eq!(
                table.attach(entry("registry.example/team")),
                entry("registry.example/team").with_credentials(credentials("puller", want)),
                "{name}"
            );
        }
        let table =
            RegistryCredentials::from_cli(&cli(&[flag("registry.example/team", &at_cap)]), me())
                .expect("at the cap");
        assert_eq!(
            table.attach(entry("registry.example/team")),
            entry("registry.example/team").with_credentials(credentials("puller", &password))
        );
    }

    /// Proves the credentials are read once: after the table is made, the file can be
    /// replaced or removed and the table still gives the credentials it read, so a later
    /// pull uses those. Catches a table that reads its file again at each use.
    #[test]
    fn the_credentials_are_read_once_at_start() {
        let dir = private_dir("registry-credentials-once");
        let path = secret(&dir, "image", b"puller:first", 0o600);
        let cli = cli(&[flag("registry.example/team", &path)]);
        let table = RegistryCredentials::from_cli(&cli, me()).expect("credentials");
        let want = entry("registry.example/team").with_credentials(credentials("puller", "first"));
        fs::write(&path, "puller:second").expect("replace");
        assert_eq!(table.attach(entry("registry.example/team")), want);
        fs::remove_file(&path).expect("remove");
        assert_eq!(table.attach(entry("registry.example/team")), want);
        assert_eq!(
            table.image_allow(&cli).expect("image allow"),
            [want],
            "the crun runtime's allow-list"
        );
    }

    /// Proves the crun runtime's allow-list is the `--image-allow` entries in order, each
    /// with its own file's credentials and the others without, and that a wrong entry
    /// names `--image-allow`. Catches credentials dropped on the way to the image client.
    #[test]
    fn the_image_allow_list_carries_the_credentials() {
        let dir = private_dir("registry-credentials-images");
        let path = secret(&dir, "image", b"puller:pw", 0o600);
        let cli = cli(&[
            "--image-allow=registry.example/other".to_owned(),
            flag("registry.example/team", &path),
        ]);
        let table = RegistryCredentials::from_cli(&cli, me()).expect("credentials");
        assert_eq!(
            table.image_allow(&cli).expect("image allow"),
            [
                entry("registry.example/team").with_credentials(credentials("puller", "pw")),
                entry("registry.example/other"),
            ]
        );
        let bare = self::cli(&["--image-allow=registry.example".to_owned()]);
        assert_eq!(
            table.image_allow(&bare).expect_err("a bare host"),
            "--image-allow: \"registry.example\" is not a well formed registry reference"
        );
    }

    /// Proves the table prints no secret: its `Debug` shows the entries and a redacted
    /// marker for each file's credentials, and the flags' `Debug` holds only the path.
    /// Catches a derived `Debug` that prints the header js-oci sends.
    #[test]
    fn credentials_do_not_appear_in_debug_or_errors() {
        let dir = private_dir("registry-credentials-debug");
        let path = secret(&dir, "image", b"puller:s3cret-pw", 0o600);
        let cli = cli(&[flag("registry.example/team", &path)]);
        let table = RegistryCredentials::from_cli(&cli, me()).expect("credentials");
        assert_eq!(
            format!("{table:?}"),
            "RegistryCredentials { entries: [(Allow { authority: Authority { host: \
             \"registry.example\", port: 443 }, prefix: \"team\", private: false, loopback: \
             false, plain_http: false, port_named: false, credentials: None }, \
             Credentials(<redacted>))] }"
        );
        assert_eq!(
            format!("{:?}", cli.registry_credentials),
            format!("[\"registry.example/team={}\"]", path.display())
        );
    }
}
