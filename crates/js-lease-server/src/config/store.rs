//! The `store` setting (`--store`, `server.store`) and its SQLite gate (`--allow-sqlite-store`,
//! `server.allow_sqlite_store`).
//!
//! The setting is the path of a file that holds the store's connection and nothing else, as
//! the other secret-bearing settings are: the file holds the secret, never the flag. The file
//! is one line, `postgres://...` or `postgresql://...` for Postgres, or `sqlite:<path>` for the
//! SQLite backend, which is for tests and single-node development and is refused unless the
//! gate is on. A file other users can read is refused as the token files are.
//!
//! No refusal repeats the file's content: a connection URL carries a password, so a URL that
//! does not parse is refused by kind ("does not parse"), never by the parser's text.

use std::path::Path;

use js_state::WriterTarget;
use js_state::postgres::{ConfigError, PostgresConfig};

use super::{Cli, read_private};
use crate::error::StartupError;
use crate::kinds::SecretFiles;

/// What the file must hold, said by every refusal of its shape.
const ONE_LINE: &str =
    "must hold one line: postgres://..., postgresql://... or sqlite:<absolute path>";

/// The name the server gives its connections, as `pg_stat_activity` shows it.
const APPLICATION_NAME: &str = "js-lease-server";

/// Where the store setting came from, which decides how its refusals name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreNames {
    /// `--store` and `--allow-sqlite-store`.
    Flags,
    /// `server.store` and `server.allow_sqlite_store` of the configuration file.
    File,
}

impl StoreNames {
    /// A refusal of the store setting for `reason`, naming the flag or the key.
    pub(crate) fn refuse(self, reason: impl std::fmt::Display) -> StartupError {
        match self {
            Self::Flags => StartupError::new("--store", reason),
            Self::File => StartupError::at("server.store", reason),
        }
    }

    /// How the SQLite gate is turned on, as a refusal says it.
    fn gate(self) -> &'static str {
        match self {
            Self::Flags => "--allow-sqlite-store",
            Self::File => "server.allow_sqlite_store = true",
        }
    }
}

/// The store the setting names, with the names its refusals use.
#[derive(Debug)]
pub(crate) struct StoreSetting {
    /// The database or the SQLite file. Its `Debug` leaves the password out.
    pub(crate) target: WriterTarget,
    /// `--store` or `server.store`.
    pub(crate) names: StoreNames,
}

/// The store `--store` names, read as `--token-file` is (owner-only); `None` without the
/// flag. `--allow-sqlite-store` without `--store` is refused: it would do nothing.
pub(super) fn from_flags(cli: &Cli) -> Result<Option<StoreSetting>, StartupError> {
    let Some(path) = &cli.store else {
        return match cli.allow_sqlite_store {
            true => Err(StartupError::new(
                "--allow-sqlite-store",
                "requires --store",
            )),
            false => Ok(None),
        };
    };
    let bytes = read_private(path, "--store")?;
    parse(&bytes, path, cli.allow_sqlite_store, StoreNames::Flags).map(Some)
}

/// The store `server.store` names, read through `files` as the file's other secrets are;
/// `None` when the key is absent (the file reader refuses `allow_sqlite_store = true`
/// without it).
pub(super) fn from_file(
    path: Option<&Path>,
    allow_sqlite: bool,
    files: &dyn SecretFiles,
) -> Result<Option<StoreSetting>, StartupError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let names = StoreNames::File;
    let bytes = files.read(path).map_err(|reason| names.refuse(reason))?;
    parse(&bytes, path, allow_sqlite, names).map(Some)
}

/// Reads the content `bytes` of the store file `path`: one line, blank lines and the
/// whitespace around it aside. A `sqlite:` store needs `allow_sqlite`.
fn parse(
    bytes: &[u8],
    path: &Path,
    allow_sqlite: bool,
    names: StoreNames,
) -> Result<StoreSetting, StartupError> {
    let shape = || names.refuse(format!("{} {ONE_LINE}", path.display()));
    let text = std::str::from_utf8(bytes).map_err(|_| shape())?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err(shape());
    };
    let target = if let Some(file) = line.strip_prefix("sqlite:") {
        sqlite(Path::new(file), path, allow_sqlite, names)?
    } else if line.starts_with("postgres://") || line.starts_with("postgresql://") {
        let config = PostgresConfig::parse(line)
            .map_err(|err| names.refuse(format!("{}: {}", path.display(), unquoted(&err))))?;
        WriterTarget::Postgres(Box::new(config.with_application_name(APPLICATION_NAME)))
    } else {
        return Err(shape());
    };
    Ok(StoreSetting { target, names })
}

/// The store the content `bytes` of the store file `path` names, read for the update hook:
/// as `--store` reads it, a `sqlite:` store accepted (the hook only probes a store, and the
/// server's own start holds it to the gate).
pub(crate) fn hook_store_target(bytes: &[u8], path: &Path) -> Result<WriterTarget, StartupError> {
    parse(bytes, path, true, StoreNames::Flags).map(|setting| setting.target)
}

/// The SQLite store at `file`, which the store file `path` names: refused without the gate,
/// and refused when it is not an absolute path (the server's working directory is not
/// configuration).
fn sqlite(
    file: &Path,
    path: &Path,
    allow_sqlite: bool,
    names: StoreNames,
) -> Result<WriterTarget, StartupError> {
    if !allow_sqlite {
        let gate = names.gate();
        return Err(names.refuse(format!(
            "{} names a sqlite: store, which is accepted only with {gate}: SQLite is for tests and single-node development",
            path.display()
        )));
    }
    match file.is_absolute() {
        true => Ok(WriterTarget::Sqlite(file.to_path_buf())),
        false => Err(names.refuse(format!("{} {ONE_LINE}", path.display()))),
    }
}

/// Why a Postgres URL was refused, without any part of it: the parser's own text can quote
/// the value it stopped at, so a URL that does not parse is said to be one, and the other
/// refusals are fixed texts.
fn unquoted(err: &ConfigError) -> String {
    match err {
        ConfigError::Invalid(_) => "the connection URL does not parse".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, allow_sqlite: bool) -> Result<StoreSetting, String> {
        parse(
            text.as_bytes(),
            Path::new("/etc/js/store"),
            allow_sqlite,
            StoreNames::Flags,
        )
        .map_err(|err| err.to_string())
    }

    /// Asserts that one line of either Postgres scheme is read, with blank lines and the
    /// whitespace around the line ignored, and that the URL's password is not in the
    /// setting's `Debug`.
    #[test]
    fn a_postgres_url_on_one_line_is_read() {
        for text in [
            "postgres://js:pw-0123@db.example.org/js",
            "postgresql://js:pw-0123@db.example.org/js\n",
            "\n  postgres://js:pw-0123@db.example.org/js  \n\n",
        ] {
            let setting = read(text, false).unwrap();
            let shown = format!("{:?}", setting.target);
            assert!(shown.starts_with("Postgres(PostgresConfig {"), "{text:?}");
            assert!(!format!("{setting:?}").contains("pw-0123"), "{text:?}");
        }
    }

    /// Asserts that a file of another shape is refused by what it must hold, without its
    /// content: empty, two lines, another scheme, bytes that are not UTF-8, a relative or
    /// empty SQLite path.
    #[test]
    fn a_file_of_another_shape_is_refused_without_its_content() {
        let must = format!("--store: /etc/js/store {ONE_LINE}");
        for text in [
            "",
            " \n\n",
            "postgres://a@h/d\npostgres://b@h/d",
            "mysql://js:pw-0123@db.example.org/js",
            "sqlite:relative/state.db",
            "sqlite:",
        ] {
            assert_eq!(
                read(text, true).err().as_deref(),
                Some(must.as_str()),
                "{text:?}"
            );
        }
        let bytes = [0x70, 0xff, 0xfe];
        let refused = parse(&bytes, Path::new("/etc/js/store"), true, StoreNames::Flags);
        assert_eq!(refused.err().map(|err| err.to_string()), Some(must));
    }

    /// Asserts that a Postgres URL that does not parse, or asks to verify the server's
    /// certificate with no root certificate to check it against, is refused without
    /// repeating any part of it, and that a URL asking for TLS is read.
    #[test]
    fn a_postgres_url_that_is_refused_is_not_repeated() {
        let cases = [
            (
                "postgres://js:pw-0123@db.example.org:port-9/js",
                "the connection URL does not parse",
            ),
            (
                "postgres://js:pw-0123@db.example.org/js?sslmode=verify-full",
                "the state store's sslmode verify-ca and verify-full need a root certificate (sslrootcert)",
            ),
        ];
        for (text, reason) in cases {
            let said = read(text, false).err().unwrap();
            assert_eq!(said, format!("--store: /etc/js/store: {reason}"));
        }
        let tls = read(
            "postgres://js:pw-0123@db.example.org/js?sslmode=require",
            false,
        );
        assert!(tls.is_ok(), "{:?}", tls.err());
    }

    /// Asserts that a `sqlite:` store is refused without the gate, naming the flag or the
    /// key that turns it on and saying what SQLite is for, and read with it.
    #[test]
    fn sqlite_needs_the_gate() {
        let said = read("sqlite:/var/lib/js/state.db", false).err().unwrap();
        assert_eq!(
            said,
            "--store: /etc/js/store names a sqlite: store, which is accepted only with --allow-sqlite-store: SQLite is for tests and single-node development"
        );
        let file = parse(
            b"sqlite:/var/lib/js/state.db",
            Path::new("/etc/js/store"),
            false,
            StoreNames::File,
        );
        assert_eq!(
            file.err().map(|err| err.to_string()).as_deref(),
            Some(
                "server.store: /etc/js/store names a sqlite: store, which is accepted only with server.allow_sqlite_store = true: SQLite is for tests and single-node development"
            )
        );
        let setting = read("sqlite:/var/lib/js/state.db", true).unwrap();
        let shown = format!("{:?}", setting.target);
        assert_eq!(shown, "Sqlite(\"/var/lib/js/state.db\")");
    }
}
