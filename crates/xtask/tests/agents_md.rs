//! `AGENTS.md` carries the shared core of the engineering rules word for word, and
//! `CLAUDE.md` imports `AGENTS.md`.
//!
//! The shared core is the text between a line `<!-- shared-core:begin -->` and a line
//! `<!-- shared-core:end -->` in `AGENTS.md`. Its canonical copy is
//! `docs/agents/shared-core.md`, and `docs/agents/shared-core.sha256` records the copy's
//! SHA-256 in `sha256sum` format. The section must equal the copy byte for byte, and the
//! copy must match the recorded hash, so changing the core means changing all three in one
//! pull request. `CLAUDE.md` must hold the line `@AGENTS.md` outside a fenced code block
//! (Claude Code does not read an import inside one).
//!
//! The first three tests check the repository's files. The rest feed each check made-up
//! text and pin what it accepts and what it refuses.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The line that opens the shared core.
const BEGIN: &str = "<!-- shared-core:begin -->";
/// The line that closes the shared core.
const END: &str = "<!-- shared-core:end -->";
/// The import line `CLAUDE.md` must hold.
const IMPORT: &str = "@AGENTS.md";
/// The file name the hash record names.
const COPY_NAME: &str = "shared-core.md";

/// Why a check refused its input.
#[derive(Debug, PartialEq, Eq)]
enum Fault {
    /// No line is the begin marker.
    MissingBegin,
    /// No line is the end marker.
    MissingEnd,
    /// More than one line is the begin marker.
    RepeatedBegin,
    /// More than one line is the end marker.
    RepeatedEnd,
    /// The end marker comes before the begin marker.
    EndBeforeBegin,
    /// The section and the canonical copy first differ at `line` (counted from 1 inside
    /// the section); each side is that line with its line ending, `None` past the end.
    Drift {
        line: usize,
        section: Option<String>,
        canonical: Option<String>,
    },
    /// The hash record is not one `<64 lowercase hex> shared-core.md` line.
    HashFormat(String),
    /// The copy's SHA-256 is not the recorded one.
    HashMismatch { recorded: String, actual: String },
    /// No line of `CLAUDE.md` outside a fenced block is the import.
    NoImport,
}

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The file at `rel` under the repository root.
fn read(rel: &str) -> String {
    std::fs::read_to_string(repo().join(rel)).expect(rel)
}

/// `line` without its line ending.
fn bare(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// The text strictly between the begin and end marker lines of `agents`, line endings kept.
fn section(agents: &str) -> Result<String, Fault> {
    let lines: Vec<&str> = agents.split_inclusive('\n').collect();
    let at = |marker: &str| -> Vec<usize> {
        (0..lines.len())
            .filter(|&i| bare(lines[i]) == marker)
            .collect()
    };
    let (begins, ends) = (at(BEGIN), at(END));
    let begin = match begins.as_slice() {
        [] => return Err(Fault::MissingBegin),
        [one] => *one,
        _ => return Err(Fault::RepeatedBegin),
    };
    let end = match ends.as_slice() {
        [] => return Err(Fault::MissingEnd),
        [one] => *one,
        _ => return Err(Fault::RepeatedEnd),
    };
    if end < begin {
        return Err(Fault::EndBeforeBegin);
    }
    Ok(lines[begin + 1..end].concat())
}

/// Whether `section` equals `canonical`, and if not, the first line where they differ.
fn drift(section: &str, canonical: &str) -> Result<(), Fault> {
    let ours: Vec<&str> = section.split_inclusive('\n').collect();
    let theirs: Vec<&str> = canonical.split_inclusive('\n').collect();
    for i in 0..ours.len().max(theirs.len()) {
        let (a, b) = (ours.get(i), theirs.get(i));
        if a != b {
            return Err(Fault::Drift {
                line: i + 1,
                section: a.map(|s| s.to_string()),
                canonical: b.map(|s| s.to_string()),
            });
        }
    }
    Ok(())
}

/// The hex digest a `sha256sum`-format `record` gives for the copy.
fn recorded_hash(record: &str) -> Result<&str, Fault> {
    let mut words = record.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(hex), Some(COPY_NAME), None)
            if hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(hex)
        }
        _ => Err(Fault::HashFormat(record.to_string())),
    }
}

/// Whether `copy` has the SHA-256 that `record` records.
fn hash_matches(copy: &str, record: &str) -> Result<(), Fault> {
    let recorded = recorded_hash(record)?;
    let actual = hex::encode(Sha256::digest(copy.as_bytes()));
    if recorded != actual {
        return Err(Fault::HashMismatch {
            recorded: recorded.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Whether `claude` holds the import line outside every fenced code block.
fn imports(claude: &str) -> Result<(), Fault> {
    let mut fenced = false;
    for line in claude.lines() {
        let start = line.trim_start();
        if start.starts_with("```") || start.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && line.trim_end() == IMPORT {
            return Ok(());
        }
    }
    Err(Fault::NoImport)
}

/// The SHA-256 record of `copy`, as `sha256sum` writes it.
fn record_of(copy: &str) -> String {
    format!(
        "{}  {COPY_NAME}\n",
        hex::encode(Sha256::digest(copy.as_bytes()))
    )
}

#[test]
fn the_shared_core_of_agents_md_equals_its_canonical_copy() {
    let core = section(&read("AGENTS.md")).expect("AGENTS.md has one shared core");
    assert!(core.contains("## Shared core\n"), "{core}");
    assert_eq!(drift(&core, &read("docs/agents/shared-core.md")), Ok(()));
}

#[test]
fn the_canonical_copy_matches_its_recorded_sha256() {
    assert_eq!(
        hash_matches(
            &read("docs/agents/shared-core.md"),
            &read("docs/agents/shared-core.sha256"),
        ),
        Ok(())
    );
}

#[test]
fn claude_md_imports_agents_md() {
    assert_eq!(imports(&read("CLAUDE.md")), Ok(()));
}

#[test]
fn the_section_is_the_text_strictly_between_the_markers() {
    let agents = format!("# Rules\n{BEGIN}\nA\n\nB\n{END}\n## Rust\n");
    assert_eq!(section(&agents), Ok("A\n\nB\n".to_string()));
}

#[test]
fn adjacent_markers_give_an_empty_section() {
    assert_eq!(section(&format!("{BEGIN}\n{END}\n")), Ok(String::new()));
}

#[test]
fn an_end_marker_on_the_last_line_without_a_newline_closes_the_section() {
    assert_eq!(
        section(&format!("{BEGIN}\nA\n{END}")),
        Ok("A\n".to_string())
    );
}

#[test]
fn crlf_marker_lines_are_markers_and_the_section_keeps_its_crlf() {
    let agents = format!("{BEGIN}\r\nA\r\n{END}\r\n");
    assert_eq!(section(&agents), Ok("A\r\n".to_string()));
    assert_eq!(
        drift("A\r\n", "A\n"),
        Err(Fault::Drift {
            line: 1,
            section: Some("A\r\n".to_string()),
            canonical: Some("A\n".to_string()),
        })
    );
}

#[test]
fn a_file_without_the_begin_marker_is_refused() {
    assert_eq!(section(&format!("A\n{END}\n")), Err(Fault::MissingBegin));
    assert_eq!(section(""), Err(Fault::MissingBegin));
}

#[test]
fn a_file_without_the_end_marker_is_refused() {
    assert_eq!(section(&format!("{BEGIN}\nA\n")), Err(Fault::MissingEnd));
}

#[test]
fn a_second_begin_marker_is_refused() {
    let agents = format!("{BEGIN}\nA\n{END}\n{BEGIN}\n");
    assert_eq!(section(&agents), Err(Fault::RepeatedBegin));
}

#[test]
fn a_second_end_marker_is_refused() {
    let agents = format!("{BEGIN}\nA\n{END}\nB\n{END}\n");
    assert_eq!(section(&agents), Err(Fault::RepeatedEnd));
}

#[test]
fn an_end_marker_before_the_begin_marker_is_refused() {
    let agents = format!("{END}\nA\n{BEGIN}\n");
    assert_eq!(section(&agents), Err(Fault::EndBeforeBegin));
}

#[test]
fn a_marker_with_other_text_on_its_line_is_not_a_marker() {
    let indented = format!("  {BEGIN}\nA\n{END}\n");
    assert_eq!(section(&indented), Err(Fault::MissingBegin));
    let quoted = format!("`{BEGIN}`\nA\n{END}\n");
    assert_eq!(section(&quoted), Err(Fault::MissingBegin));
    let trailing = format!("{BEGIN}\nA\n{END} x\n");
    assert_eq!(section(&trailing), Err(Fault::MissingEnd));
}

#[test]
fn an_identical_section_has_no_drift() {
    assert_eq!(drift("A\nB\n", "A\nB\n"), Ok(()));
    assert_eq!(drift("", ""), Ok(()));
}

#[test]
fn a_changed_word_is_drift_at_its_line() {
    assert_eq!(
        drift("A\nT1 BLOCK. 99%\nC\n", "A\nT1 BLOCK. 98%\nC\n"),
        Err(Fault::Drift {
            line: 2,
            section: Some("T1 BLOCK. 99%\n".to_string()),
            canonical: Some("T1 BLOCK. 98%\n".to_string()),
        })
    );
}

#[test]
fn a_dropped_last_line_is_drift() {
    assert_eq!(
        drift("A\n", "A\nB\n"),
        Err(Fault::Drift {
            line: 2,
            section: None,
            canonical: Some("B\n".to_string()),
        })
    );
}

#[test]
fn an_added_line_is_drift() {
    assert_eq!(
        drift("A\nB\n", "A\n"),
        Err(Fault::Drift {
            line: 2,
            section: Some("B\n".to_string()),
            canonical: None,
        })
    );
}

#[test]
fn a_missing_final_newline_is_drift() {
    assert_eq!(
        drift("A\nB", "A\nB\n"),
        Err(Fault::Drift {
            line: 2,
            section: Some("B".to_string()),
            canonical: Some("B\n".to_string()),
        })
    );
}

#[test]
fn an_empty_section_against_a_copy_is_drift() {
    assert_eq!(
        drift("", "A\n"),
        Err(Fault::Drift {
            line: 1,
            section: None,
            canonical: Some("A\n".to_string()),
        })
    );
}

#[test]
fn a_copy_matches_the_record_sha256sum_writes_for_it() {
    // The digest of "abc", as `printf abc | sha256sum` prints it.
    let record =
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  shared-core.md\n";
    assert_eq!(hash_matches("abc", record), Ok(()));
    assert_eq!(record_of("abc"), record);
}

#[test]
fn a_copy_changed_after_its_hash_was_recorded_is_refused() {
    let record = record_of("T1 BLOCK.\n");
    assert_eq!(
        hash_matches("T1 SHOULD.\n", &record),
        Err(Fault::HashMismatch {
            recorded: hex::encode(Sha256::digest(b"T1 BLOCK.\n")),
            actual: hex::encode(Sha256::digest(b"T1 SHOULD.\n")),
        })
    );
}

#[test]
fn a_malformed_hash_record_is_refused_by_name() {
    let digest = hex::encode(Sha256::digest(b"A\n"));
    let malformed = [
        String::new(),
        digest.clone(),
        format!("{}  {COPY_NAME}\n", digest.to_uppercase()),
        format!("{}  {COPY_NAME}\n", &digest[..63]),
        format!("{digest}0  {COPY_NAME}\n"),
        format!("{}g  {COPY_NAME}\n", &digest[..63]),
        format!("{digest}  other.md\n"),
        format!("{digest}  {COPY_NAME} extra\n"),
    ];
    for record in malformed {
        assert_eq!(
            hash_matches("A\n", &record),
            Err(Fault::HashFormat(record.clone()))
        );
    }
}

#[test]
fn the_import_line_is_found_among_other_lines() {
    assert_eq!(imports("@AGENTS.md\n"), Ok(()));
    assert_eq!(imports("@AGENTS.md"), Ok(()));
    assert_eq!(imports("# Notes\n\n@AGENTS.md\n\nMore.\n"), Ok(()));
    assert_eq!(imports("@AGENTS.md  \r\n"), Ok(()));
}

#[test]
fn the_import_after_a_closed_fence_counts() {
    assert_eq!(imports("```\nx\n```\n@AGENTS.md\n"), Ok(()));
    assert_eq!(imports("~~~\nx\n~~~\n@AGENTS.md\n"), Ok(()));
}

#[test]
fn an_import_inside_a_fenced_block_does_not_count() {
    assert_eq!(imports("```\n@AGENTS.md\n```\n"), Err(Fault::NoImport));
    assert_eq!(imports("~~~md\n@AGENTS.md\n"), Err(Fault::NoImport));
}

#[test]
fn a_mention_that_is_not_the_import_line_does_not_count() {
    for claude in [
        "",
        "Read @AGENTS.md first.\n",
        "`@AGENTS.md`\n",
        "@AGENTS.md.bak\n",
        "@agents.md\n",
        "AGENTS.md\n",
    ] {
        assert_eq!(imports(claude), Err(Fault::NoImport), "{claude:?}");
    }
}
