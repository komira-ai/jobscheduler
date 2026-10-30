//! The result lines the pull request's table is made from.

use std::io::Write as _;

/// Prints `RESULT <experiment> | <key> | <value>` and appends the line to the report file.
pub fn row(experiment: &str, key: &str, value: impl std::fmt::Display) {
    let line = format!("RESULT {experiment} | {key} | {value}");
    println!("{line}");
    if let Some(path) = std::env::var_os("JS_EGRESS_SPIKE_REPORT")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
