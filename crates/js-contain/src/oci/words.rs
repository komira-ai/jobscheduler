//! What `crun` and the container's init say when a container did not run, as one line of
//! words.
//!
//! `crun` writes its errors as JSON records to the `--log` file of its lease
//! ([`Reports::log`](super::Reports::log)); the container's init writes one plain line per
//! failure to the report file ([`Reports::init`](super::Reports::init)). Both are turned
//! into `crun: <message>` so that one reader serves both: a failure to start the program
//! reads the same whichever of the two reported it.

/// The error and fatal records of `crun`'s JSON `log`, each as `crun: <message>`,
/// joined by `; `. Warnings and lines that are not records are left out.
#[must_use]
pub fn crun_words(log: &str) -> String {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["level"] == "error" || record["level"] == "fatal")
        .filter_map(|record| record["msg"].as_str().map(|msg| format!("crun: {msg}")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The init's `report`, one `crun: <line>` for each line that is not empty, joined by `; `.
#[must_use]
pub fn init_words(report: &str) -> String {
    report
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| format!("crun: {line}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = concat!(
        r#"{"level":"warning","msg":"cgroup v1 is deprecated"}"#,
        "\n",
        r#"{"level":"error","msg":"executable file `env` not found in $PATH: No such file or directory"}"#,
        "\n",
        "not json at all\n",
        r#"{"level":"fatal","msg":"open executable: Permission denied"}"#,
        "\n",
    );

    /// Catches a warning taken for a failure (a lease with a warning in `crun`'s log is
    /// not a runtime failure), an error record lost, and a line that is not a record
    /// breaking the rest.
    #[test]
    fn crun_words_are_its_error_records() {
        assert_eq!(
            crun_words(LOG),
            "crun: executable file `env` not found in $PATH: No such file or directory; \
             crun: open executable: Permission denied"
        );
        assert_eq!(crun_words(""), "");
        assert_eq!(crun_words("{\"level\":\"warning\",\"msg\":\"x\"}\n"), "");
        assert_eq!(crun_words("{\"level\":\"error\"}\n"), "", "no message");
    }

    /// Catches the init's report being read as anything but `crun`'s words for the same
    /// failure, and an empty line becoming a word.
    #[test]
    fn the_inits_report_is_read_in_cruns_words() {
        assert_eq!(init_words(""), "");
        assert_eq!(
            init_words(
                "open executable: Permission denied\n\nexec container process `x`: Text file busy\n"
            ),
            "crun: open executable: Permission denied; \
             crun: exec container process `x`: Text file busy"
        );
    }
}
