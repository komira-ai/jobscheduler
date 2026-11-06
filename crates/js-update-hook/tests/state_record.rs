//! The record in the state directory: reading it (and refusing what is not a record of this
//! schema), the two-step atomic save, the delete, and the read that takes no lock.

mod state_support;

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use js_update_hook::{Held, RECORD_FILE, RECORD_MAX, StateError, TEMP_FILE, peek};
use state_support::{
    Note, TestReason, Unwritable, as_root, cell, fifo, open, record_of, write_record,
};

fn record_text(dir: &Path) -> String {
    fs::read_to_string(dir.join(RECORD_FILE)).unwrap()
}

/// The text of the error `load` gives for `body`, which must be a `Corrupt`.
fn corrupt_text(name: &str, body: &str) -> String {
    let tmp = cell(name);
    write_record(tmp.path(), body);
    let held = open(tmp.path()).unwrap();
    match held.load().unwrap_err() {
        StateError::Corrupt(text) => text,
        other => panic!("not corrupt: {other:?}"),
    }
}

/// The record's path is `record.json` in the directory the step was opened on.
#[test]
fn the_record_path_is_record_json_in_the_directory() {
    let tmp = cell("record-path");
    let held = open(tmp.path()).unwrap();
    assert_eq!(held.record_path(), tmp.path().join("record.json"));
}

/// A directory with no record loads as no record, and loading makes nothing.
#[test]
fn a_directory_with_no_record_loads_as_none() {
    let tmp = cell("record-none");
    let held = open(tmp.path()).unwrap();
    assert_eq!(held.load().unwrap(), None);
    assert!(!tmp.path().join(RECORD_FILE).exists());
}

/// What is saved is what is loaded; the file is pretty JSON ending in a newline and no
/// temporary file is left behind.
#[test]
fn a_saved_record_loads_back_and_leaves_no_temporary_file() {
    let tmp = cell("record-roundtrip");
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("first")).unwrap();
    assert_eq!(held.load().unwrap(), Some(Note::new("first")));
    assert_eq!(
        record_text(tmp.path()),
        "{\n  \"schema\": 3,\n  \"name\": \"first\"\n}\n"
    );
    assert!(!tmp.path().join(TEMP_FILE).exists());
}

/// A second save replaces the first, even when the new record is shorter, and a record of the
/// hook's own schema is replaceable.
#[test]
fn a_second_save_replaces_the_first_whole() {
    let tmp = cell("record-replace");
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("a much longer name than the next"))
        .unwrap();
    held.save(&Note::new("b")).unwrap();
    assert_eq!(held.load().unwrap(), Some(Note::new("b")));
    assert_eq!(
        record_text(tmp.path()),
        "{\n  \"schema\": 3,\n  \"name\": \"b\"\n}\n"
    );
}

/// The first half of a save leaves the old record as it was and the new one in the temporary
/// file, as a crash would; the second half then makes it the record and removes the temporary
/// file by renaming it.
#[test]
fn a_save_stopped_after_the_first_half_leaves_the_old_record_until_commit() {
    let tmp = cell("record-two-halves");
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("old")).unwrap();
    held.write_temp(&Note::new("new")).unwrap();
    assert_eq!(held.load().unwrap(), Some(Note::new("old")));
    assert!(tmp.path().join(TEMP_FILE).is_file());
    held.commit().unwrap();
    assert_eq!(held.load().unwrap(), Some(Note::new("new")));
    assert!(!tmp.path().join(TEMP_FILE).exists());
}

/// A second write of the temporary file truncates what a killed save left, so a short record
/// over a long leftover is not a long record with a tail.
#[test]
fn a_leftover_temporary_file_is_truncated_by_the_next_save() {
    let tmp = cell("record-temp-truncated");
    fs::write(tmp.path().join(TEMP_FILE), "x".repeat(4096)).unwrap();
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("n")).unwrap();
    assert_eq!(held.load().unwrap(), Some(Note::new("n")));
}

/// A commit with no temporary file fails as a file error naming the record, and changes
/// nothing.
#[test]
fn a_commit_without_a_temporary_file_is_an_io_error_naming_the_record() {
    let tmp = cell("record-commit-no-temp");
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("kept")).unwrap();
    let error = held.commit().unwrap_err();
    let record = tmp.path().join(RECORD_FILE);
    assert!(
        matches!(&error, StateError::Io { path, source } if *path == record && source.kind() == io::ErrorKind::NotFound),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "{}: No such file or directory (os error 2)",
            record.display()
        )
    );
    assert_eq!(held.load().unwrap(), Some(Note::new("kept")));
}

/// A record that cannot be serialised fails the save as a file error naming the temporary
/// file, with the serialiser's message, and writes nothing.
#[test]
fn a_record_that_cannot_be_serialised_fails_the_save_as_an_io_error() {
    let tmp = cell("record-unwritable");
    let held = Held::<Unwritable>::open(tmp.path(), &as_root).unwrap();
    let error = held.save(&Unwritable { schema: 3 }).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("{}: cannot serialize", tmp.path().join(TEMP_FILE).display())
    );
    assert!(matches!(error, StateError::Io { .. }));
    assert!(!tmp.path().join(TEMP_FILE).exists());
}

/// A temporary file's place taken by a link or a directory fails
/// the save as a file error naming the temporary file; nothing is followed or waited on and the
/// old record stays.
#[test]
fn a_temporary_file_that_is_not_a_regular_file_fails_the_save() {
    for kind in ["link", "dir"] {
        let tmp = cell(&format!("record-temp-odd-{kind}"));
        let temp = tmp.path().join(TEMP_FILE);
        match kind {
            "link" => symlink("/nonexistent-target", &temp).unwrap(),
            _ => fs::create_dir(&temp).unwrap(),
        }
        let held = open(tmp.path()).unwrap();
        let error = held.save(&Note::new("n")).unwrap_err();
        assert!(
            matches!(&error, StateError::Io { path, .. } if *path == temp),
            "{kind}: {error:?}"
        );
        assert!(!tmp.path().join(RECORD_FILE).exists(), "{kind}");
    }
}

/// A pipe in the temporary file's place that is being read opens, and is then refused as not a
/// regular file with that message.
#[test]
fn a_pipe_in_the_temporary_files_place_is_refused_as_not_a_regular_file() {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = cell("record-temp-fifo-message");
    let temp = tmp.path().join(TEMP_FILE);
    fifo(&temp);
    let _reader = File::options()
        .read(true)
        .custom_flags(0o4000)
        .open(&temp)
        .unwrap();
    let held = open(tmp.path()).unwrap();
    let error = held.write_temp(&Note::new("n")).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("{}: not a regular file", temp.display())
    );
}

/// A pipe in the record's place is refused as not a record, without waiting for a writer.
#[test]
fn a_pipe_in_the_records_place_is_corrupt_and_is_not_waited_on() {
    let tmp = cell("record-fifo");
    fifo(&tmp.path().join(RECORD_FILE));
    let held = open(tmp.path()).unwrap();
    let error = held.load().unwrap_err();
    assert_eq!(
        error.to_string(),
        "the record is not readable: not a regular file"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
}

/// A directory in the record's place is refused as not a record.
#[test]
fn a_directory_in_the_records_place_is_corrupt() {
    let tmp = cell("record-dir");
    fs::create_dir(tmp.path().join(RECORD_FILE)).unwrap();
    let held = open(tmp.path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Corrupt(text)) if text == "not a regular file"));
}

/// A link in the record's place is not followed: it is a file error naming the record.
#[test]
fn a_link_in_the_records_place_is_not_followed() {
    let tmp = cell("record-link");
    let target = tmp.path().join("elsewhere.json");
    fs::write(&target, record_of(3)).unwrap();
    let record = tmp.path().join(RECORD_FILE);
    symlink(&target, &record).unwrap();
    let held = open(tmp.path()).unwrap();
    let error = held.load().unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if *path == record),
        "{error:?}"
    );
}

/// A record of more than `RECORD_MAX` bytes is refused with that limit in the message; one of
/// exactly `RECORD_MAX` bytes is read.
#[test]
fn a_record_over_the_size_limit_is_corrupt_and_one_at_the_limit_is_read() {
    let tmp = cell("record-size");
    let held = open(tmp.path()).unwrap();
    let body = record_of(3);
    let at_limit = format!(
        "{body}{}",
        " ".repeat(usize::try_from(RECORD_MAX).unwrap() - body.len())
    );
    assert_eq!(at_limit.len() as u64, RECORD_MAX);
    write_record(tmp.path(), &at_limit);
    assert_eq!(held.load().unwrap(), Some(Note::new("kept")));

    write_record(tmp.path(), &format!("{at_limit} "));
    let error = held.load().unwrap_err();
    assert_eq!(
        error.to_string(),
        "the record is not readable: more than 65536 bytes"
    );
}

/// What is not JSON, or is JSON without a schema, is refused as corrupt with the parser's
/// own message.
#[test]
fn a_record_that_is_not_json_or_has_no_schema_is_corrupt() {
    assert_eq!(
        corrupt_text("record-not-json", "this is not json"),
        "expected ident at line 1 column 2"
    );
    assert_eq!(
        corrupt_text("record-empty", ""),
        "EOF while parsing a value at line 1 column 0"
    );
    assert_eq!(
        corrupt_text("record-no-schema", "{\"name\": \"x\"}"),
        "missing field `schema` at line 1 column 13"
    );
}

/// A record of a newer schema is not loaded: the error names the schema found and the one this
/// hook writes.
#[test]
fn a_record_of_a_newer_schema_is_read_only() {
    let tmp = cell("record-newer");
    write_record(tmp.path(), &record_of(4));
    let held = open(tmp.path()).unwrap();
    let error = held.load().unwrap_err();
    assert!(matches!(error, StateError::Newer { found: 4 }), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the record has schema 4, newer than 3: read-only to this hook"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
}

/// A newer record is judged by its schema alone: fields this hook does not know are not a
/// reason to call it corrupt.
#[test]
fn a_newer_record_with_fields_unknown_to_the_hook_is_still_newer() {
    let tmp = cell("record-newer-fields");
    write_record(tmp.path(), "{\"schema\": 9, \"future\": [1, 2, 3]}");
    let held = open(tmp.path()).unwrap();
    assert!(matches!(held.load(), Err(StateError::Newer { found: 9 })));
}

/// A record of an older schema is refused as one this hook did not write, naming the schema.
#[test]
fn a_record_of_an_older_schema_is_corrupt() {
    assert_eq!(
        corrupt_text("record-older", &record_of(2)),
        "schema 2 is not one this hook wrote"
    );
    assert_eq!(
        corrupt_text("record-schema-zero", &record_of(0)),
        "schema 0 is not one this hook wrote"
    );
}

/// A record of this schema whose body the hook's type refuses (an unknown field, a wrong
/// type, a missing field) is corrupt with the deserialiser's message.
#[test]
fn a_record_of_this_schema_with_a_body_the_type_refuses_is_corrupt() {
    assert_eq!(
        corrupt_text(
            "record-unknown-field",
            "{\"schema\": 3, \"name\": \"x\", \"extra\": 1}"
        ),
        "unknown field `extra`, expected `schema` or `name` at line 1 column 34"
    );
    assert_eq!(
        corrupt_text("record-wrong-type", "{\"schema\": 3, \"name\": 7}"),
        "invalid type: integer `7`, expected a string at line 1 column 23"
    );
    assert_eq!(
        corrupt_text("record-missing-field", "{\"schema\": 3}"),
        "missing field `name` at line 1 column 13"
    );
}

/// A save, the first half, the second half, and a delete each refuse to touch a record that a
/// load refuses (newer, corrupt), and leave it and the directory as they were.
#[test]
fn nothing_replaces_or_deletes_a_record_that_is_refused() {
    for (name, body) in [("newer", record_of(4)), ("corrupt", "garbage".to_owned())] {
        let tmp = cell(&format!("record-refused-{name}"));
        write_record(tmp.path(), &body);
        let held = open(tmp.path()).unwrap();
        let note = Note::new("n");
        let results = [
            held.save(&note).unwrap_err(),
            held.write_temp(&note).unwrap_err(),
            held.commit().unwrap_err(),
            held.delete().unwrap_err(),
        ];
        for error in results {
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&held.load().unwrap_err()),
                "{name}: {error:?}"
            );
        }
        assert_eq!(record_text(tmp.path()), body, "{name}");
        assert!(!tmp.path().join(TEMP_FILE).exists(), "{name}");
    }
}

/// A delete of a directory with no record does nothing and succeeds.
#[test]
fn deleting_where_there_is_no_record_does_nothing() {
    let tmp = cell("delete-none");
    let held = open(tmp.path()).unwrap();
    held.delete().unwrap();
    assert_eq!(held.load().unwrap(), None);
}

/// A delete removes the record and the temporary file a killed save left, and nothing else
/// (the lock file stays).
#[test]
fn deleting_removes_the_record_and_a_leftover_temporary_file() {
    let tmp = cell("delete-both");
    let held = open(tmp.path()).unwrap();
    held.save(&Note::new("n")).unwrap();
    held.write_temp(&Note::new("m")).unwrap();
    held.delete().unwrap();
    assert!(!tmp.path().join(RECORD_FILE).exists());
    assert!(!tmp.path().join(TEMP_FILE).exists());
    assert!(tmp.path().join("lock").is_file());
    assert_eq!(held.load().unwrap(), None);
}

/// A delete after a save killed before its rename removes the leftover temporary file though
/// there is no record.
#[test]
fn deleting_removes_a_temporary_file_when_there_is_no_record() {
    let tmp = cell("delete-temp-only");
    let held = open(tmp.path()).unwrap();
    held.write_temp(&Note::new("n")).unwrap();
    held.delete().unwrap();
    assert!(!tmp.path().join(TEMP_FILE).exists());
}

/// A name that cannot be unlinked (a directory with something in it in the temporary file's
/// place) fails the delete as a file error naming that file.
#[test]
fn a_delete_that_cannot_unlink_a_name_is_an_io_error_naming_it() {
    let tmp = cell("delete-cannot-unlink");
    let temp = tmp.path().join(TEMP_FILE);
    fs::create_dir(&temp).unwrap();
    fs::write(temp.join("inside"), "x").unwrap();
    let held = open(tmp.path()).unwrap();
    let error = held.delete().unwrap_err();
    assert!(
        matches!(&error, StateError::Io { path, .. } if *path == temp),
        "{error:?}"
    );
    assert_eq!(error.reason(), TestReason::StateInvalid);
}

/// The record is read without the lock, without making anything, and while another step holds
/// the directory: the whole record, as saved.
#[test]
fn peek_reads_the_record_while_another_step_holds_the_lock() {
    let tmp = cell("peek-while-held");
    let held = open(tmp.path()).unwrap();
    assert_eq!(peek::<Note>(tmp.path(), &as_root).unwrap(), None);
    held.save(&Note::new("seen")).unwrap();
    assert_eq!(
        peek::<Note>(tmp.path(), &as_root).unwrap(),
        Some(Note::new("seen"))
    );
    held.save(&Note::new("later")).unwrap();
    assert_eq!(
        peek::<Note>(tmp.path(), &as_root).unwrap(),
        Some(Note::new("later"))
    );
}

/// A peek at a directory with no record is no record, and makes no file.
#[test]
fn peek_at_a_directory_with_no_record_is_none() {
    let tmp = cell("peek-none");
    assert_eq!(peek::<Note>(tmp.path(), &as_root).unwrap(), None);
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// A peek refuses a record as a load does: newer, older, a pipe, a body the type refuses.
#[test]
fn peek_refuses_a_record_as_load_does() {
    let tmp = cell("peek-refused");
    write_record(tmp.path(), &record_of(4));
    let error = peek::<Note>(tmp.path(), &as_root).unwrap_err();
    assert!(matches!(error, StateError::Newer { found: 4 }), "{error:?}");
    write_record(tmp.path(), &record_of(1));
    let error = peek::<Note>(tmp.path(), &as_root).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the record is not readable: schema 1 is not one this hook wrote"
    );
    fs::remove_file(tmp.path().join(RECORD_FILE)).unwrap();
    fifo(&tmp.path().join(RECORD_FILE));
    let error = peek::<Note>(tmp.path(), &as_root).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the record is not readable: not a regular file"
    );
}
