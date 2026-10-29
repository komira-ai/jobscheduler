//! The `static` kind of a service's table of provider kinds: a pool whose hosts an
//! operator lists in a file.
//!
//! A pool of this kind is written in the service's configuration file as
//!
//! ```toml
//! provider = { kind = "static", hosts_file = "/etc/jobscheduler/hosts.toml" }
//! ```
//!
//! and the file it names has the format of [`EXAMPLE_CONFIG`](crate::EXAMPLE_CONFIG).
//! [`build`] reads the table the service hands it (everything but `kind`) and checks the
//! file offline, so a start with a bad hosts file is refused naming the pool and the field.

use std::fmt;
use std::path::PathBuf;

use crate::config;

/// A table that [`build`] refused: the field at fault, relative to the provider table,
/// and why. The reason holds nothing the table said except what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The field at fault, such as `hosts_file`.
    pub field: String,
    /// What is wrong with it.
    pub reason: String,
}

fn refused(field: &str, reason: impl fmt::Display) -> Refused {
    Refused {
        field: field.to_owned(),
        reason: reason.to_string(),
    }
}

/// The fields of a `static` provider table (`kind` is the service's and is not passed).
const KNOWN: &str = "hosts_file";

/// The path of the hosts file `table` names, after checking that the file can be read and
/// lists valid hosts.
///
/// # Errors
///
/// [`Refused`] naming the field when the table has a field other than `hosts_file`, has no
/// `hosts_file` or one that is not a string, or names a file that cannot be read or holds
/// no valid host list.
pub fn build(table: &toml::Table) -> Result<PathBuf, Refused> {
    if let Some(unknown) = table.keys().find(|key| key.as_str() != KNOWN) {
        return Err(refused(
            unknown,
            format_args!("unknown field; known: {KNOWN}"),
        ));
    }
    let path = match table.get(KNOWN) {
        None => return Err(refused(KNOWN, "required")),
        Some(toml::Value::String(path)) => PathBuf::from(path),
        Some(_) => return Err(refused(KNOWN, "must be a string")),
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| refused(KNOWN, format_args!("{}: {err}", path.display())))?;
    config::parse(&text)
        .map_err(|err| refused(KNOWN, format_args!("{}: {err}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EXAMPLE_CONFIG;

    /// A file in the temp directory, removed when the guard drops.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(key: &str, text: &str) -> Self {
            let name = format!("js-pool-static-kind-{key}-{}.toml", std::process::id());
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, text).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    fn refusal(text: &str) -> (String, String) {
        let refused = build(&table(text)).unwrap_err();
        (refused.field, refused.reason)
    }

    /// Asserts that a table naming a readable, valid hosts file builds to that path. It
    /// catches a kind that reads the file and loses its path.
    #[test]
    fn a_table_naming_a_valid_hosts_file_gives_its_path() {
        let file = TempFile::new("ok", EXAMPLE_CONFIG);
        let text = format!("hosts_file = {:?}", file.0.display().to_string());
        assert_eq!(build(&table(&text)).unwrap(), file.0);
    }

    /// Asserts that each way the table can be wrong is refused naming its field: an
    /// unknown field (with the known one), a missing `hosts_file`, one of the wrong type,
    /// a file that is not there, and a file that lists no valid host. It catches a typo
    /// read as a default and a refusal that names nothing.
    #[test]
    fn each_fault_of_the_table_names_its_field() {
        let (field, reason) = refusal("hosts_fil = \"/x\"");
        assert_eq!(field, "hosts_fil");
        assert_eq!(reason, "unknown field; known: hosts_file");
        let (field, reason) = refusal("");
        assert_eq!(
            (field.as_str(), reason.as_str()),
            ("hosts_file", "required")
        );
        let (field, reason) = refusal("hosts_file = 7");
        assert_eq!(
            (field.as_str(), reason.as_str()),
            ("hosts_file", "must be a string")
        );
        let (field, reason) = refusal("hosts_file = \"/js-pool-static-absent/hosts.toml\"");
        assert_eq!(field, "hosts_file");
        assert!(
            reason.starts_with("/js-pool-static-absent/hosts.toml: "),
            "{reason}"
        );
        let empty = TempFile::new("empty", "");
        let (field, reason) = refusal(&format!("hosts_file = {:?}", empty.0.display().to_string()));
        assert_eq!(field, "hosts_file");
        assert!(reason.ends_with("the pool lists no host"), "{reason}");
    }
}
