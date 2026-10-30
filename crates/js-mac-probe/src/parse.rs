//! Readers for the text the probes get back from system tools and from the sandbox trace.
//! Pure functions, tested everywhere.

/// The first double-quoted string on `line`.
#[must_use]
pub fn quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let len = line[start..].find('"')?;
    Some(&line[start..start + len])
}

/// The paths a sandbox trace asks to read that no grant covers: a `literal` match or a
/// `subpath` (the path itself or anything below it). Sorted, without repeats.
#[must_use]
pub fn unlisted_reads(trace: &str, subpaths: &[&str], literals: &[&str]) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for line in trace.lines().map(str::trim) {
        if !line.starts_with("(allow file-read") {
            continue;
        }
        let Some(path) = quoted(line) else {
            continue;
        };
        let under = |sub: &&str| {
            path == *sub
                || path
                    .strip_prefix(*sub)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        if !literals.contains(&path) && !subpaths.iter().any(under) {
            found.insert(path.to_owned());
        }
    }
    found.into_iter().collect()
}

/// The `<string>` values that directly follow `<key>key</key>` in plist text, in order.
#[must_use]
pub fn plist_strings(text: &str, key: &str) -> Vec<String> {
    let marker = format!("<key>{key}</key>");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&marker) {
        rest = &rest[at + marker.len()..];
        let value = rest
            .trim_start()
            .strip_prefix("<string>")
            .and_then(|body| body.find("</string>").map(|end| &body[..end]));
        if let Some(value) = value {
            found.push(value.to_owned());
        }
    }
    found
}

/// The first whitespace-separated token that names a `/dev/disk` device.
#[must_use]
pub fn first_device(text: &str) -> Option<String> {
    let mut tokens = text.split_whitespace();
    tokens
        .find(|t| t.starts_with("/dev/disk"))
        .map(str::to_owned)
}

/// The paths of the `LC_RPATH` commands in `otool -l` output.
#[must_use]
pub fn rpaths(otool_l: &str) -> Vec<String> {
    let lines: Vec<&str> = otool_l.lines().map(str::trim).collect();
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if *line != "cmd LC_RPATH" {
            continue;
        }
        let path = lines.get(at + 2).and_then(|l| l.strip_prefix("path "));
        if let Some(path) = path {
            found.push(
                path.split_once(" (offset")
                    .map_or(path, |(p, _)| p)
                    .to_owned(),
            );
        }
    }
    found
}

/// The install names in `otool -L` output (the first line names the file and is skipped;
/// version suffixes are dropped).
#[must_use]
pub fn dependents(otool_big_l: &str) -> Vec<String> {
    let lines = otool_big_l.lines().skip(1).map(str::trim);
    let named = lines.filter(|l| !l.is_empty());
    named
        .map(|l| l.split_once(" (").map_or(l, |(name, _)| name).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_takes_the_first_string_and_none_when_unterminated_or_absent() {
        assert_eq!(quoted(r#"(literal "/a/b") "c""#), Some("/a/b"));
        assert_eq!(quoted("no quotes"), None);
        assert_eq!(quoted(r#"one " only"#), None);
    }

    const TRACE: &str = r#"(version 1)
(allow file-read-metadata (literal "/private"))
(allow file-read-data (literal "/dev/null"))
(allow file-read-data (subpath "/usr/lib"))
(allow file-read-data (literal "/usr/lib/libSystem.B.dylib"))
(allow file-read-data (literal "/usr/library/x"))
(allow file-read-data (literal "/usr/lib"))
(allow file-read-data (literal "/Library/Preferences/a"))
(allow file-read-data (literal "/Library/Preferences/a"))
(allow file-read-data)
(allow process-exec (literal "/Library/other"))
(deny file-read-data (literal "/Library/denied"))
"#;

    #[test]
    fn unlisted_reads_keeps_only_ungranted_reads_sorted_and_once() {
        let got = unlisted_reads(TRACE, &["/usr/lib"], &["/dev/null"]);
        assert_eq!(
            got,
            ["/Library/Preferences/a", "/private", "/usr/library/x"]
        );
    }

    #[test]
    fn unlisted_reads_of_an_empty_trace_is_empty() {
        assert!(unlisted_reads("", &[], &[]).is_empty());
    }

    const PLIST: &str = "<dict>\n<key>mount-point</key>\n<string>/Volumes/a</string>\n\
<key>mount-point</key><array></array>\n<key>mount-point</key>\n<string>/Volumes/b</string>\n\
<key>mount-point</key>\n<string>cut";

    #[test]
    fn plist_strings_takes_only_string_values_that_close() {
        assert_eq!(
            plist_strings(PLIST, "mount-point"),
            ["/Volumes/a", "/Volumes/b"]
        );
        assert!(plist_strings(PLIST, "other").is_empty());
    }

    #[test]
    fn first_device_finds_a_disk_token() {
        assert_eq!(
            first_device("noise\n/dev/disk4   \t\n/dev/disk4s1"),
            Some("/dev/disk4".to_owned())
        );
        assert_eq!(first_device("/dev/null nothing"), None);
    }

    #[test]
    fn rpaths_reads_the_path_two_lines_after_each_rpath_command() {
        let text = "Load command 9\n          cmd LC_RPATH\n      cmdsize 32\n         path @loader_path/../lib (offset 12)\n\
Load command 10\n          cmd LC_RPATH\n      cmdsize 32\n         path /abs\n\
Load command 11\n          cmd LC_RPATH\n";
        assert_eq!(rpaths(text), ["@loader_path/../lib", "/abs"]);
        assert!(rpaths("cmd LC_UUID\n").is_empty());
    }

    #[test]
    fn dependents_skips_the_file_line_and_drops_version_suffixes() {
        let text = "/t/prog:\n\t@rpath/libouter.dylib (compatibility version 0.0.0, current version 0.0.0)\n\n\t/usr/lib/libSystem.B.dylib\n";
        assert_eq!(
            dependents(text),
            ["@rpath/libouter.dylib", "/usr/lib/libSystem.B.dylib"]
        );
    }
}
