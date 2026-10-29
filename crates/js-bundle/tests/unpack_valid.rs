//! What a good layer extracts to.

mod layer;

use layer::{Seen, Tar, extract, mode_of, snapshot};

/// Directories, files, executables and symlinks come out as written, with the modes the
/// format fixes (0755 for directories and executables, 0644 for other files).
#[test]
fn a_layer_extracts_to_the_tree_it_describes() {
    let tar = Tar::new()
        .dir("bin")
        .exec("bin/tool", b"#!/bin/sh\n")
        .symlink("bin/alias", "tool")
        .dir("lib")
        .file("lib/data.txt", b"hello\n")
        .file("lib/empty", b"")
        .symlink("lib/up", "../bin")
        .dir("share")
        .end();
    let run = extract(&tar);
    let done = run.ok();
    assert_eq!(done.entries, 8);
    assert_eq!(done.file_bytes, 16);
    assert_eq!(done.stream_bytes, tar.len() as u64);
    assert_eq!(done.diff_id, layer::diff_id_of(&tar));

    let seen = snapshot(&run.root());
    assert_eq!(seen.len(), 8);
    assert_eq!(seen["bin"], Seen::Dir);
    assert_eq!(
        seen["bin/tool"],
        Seen::File {
            exec: true,
            bytes: b"#!/bin/sh\n".to_vec()
        }
    );
    assert_eq!(seen["bin/alias"], Seen::Link("tool".into()));
    assert_eq!(
        seen["lib/data.txt"],
        Seen::File {
            exec: false,
            bytes: b"hello\n".to_vec()
        }
    );
    assert_eq!(
        seen["lib/empty"],
        Seen::File {
            exec: false,
            bytes: Vec::new()
        }
    );
    assert_eq!(seen["lib/up"], Seen::Link("../bin".into()));
    assert_eq!(seen["share"], Seen::Dir);
    assert_eq!(mode_of(&run.root().join("bin")), 0o755);
    assert_eq!(mode_of(&run.root().join("bin/tool")), 0o755);
    assert_eq!(mode_of(&run.root().join("lib/data.txt")), 0o644);
    run.scratch.assert_contained();
}

/// An archive with no entries is a valid, empty layer.
#[test]
fn an_empty_layer_extracts_to_nothing() {
    let run = extract(&Tar::new().end());
    assert_eq!(run.ok().entries, 0);
    assert!(snapshot(&run.root()).is_empty());
}

/// A path longer than a header field, written with a GNU long-name record, extracts.
#[test]
fn a_long_name_record_gives_the_path() {
    let dir = layer::deep_dir();
    let mut tar = Tar::new();
    let mut prefix = String::new();
    for part in dir.split('/') {
        prefix = if prefix.is_empty() {
            part.to_string()
        } else {
            format!("{prefix}/{part}")
        };
        tar = if prefix.len() > 100 {
            tar.long_name(prefix.as_bytes()).dir(&prefix)
        } else {
            tar.dir(&prefix)
        };
    }
    let file = format!("{dir}/file.txt");
    let tar = tar.long_file(&file, b"deep\n").end();
    let run = extract(&tar);
    run.ok();
    assert_eq!(std::fs::read(run.root().join(&file)).unwrap(), b"deep\n");
}

/// A PAX record may give the path of an entry whose header holds only its first bytes.
#[test]
fn a_pax_path_record_gives_the_path() {
    let path = "a".repeat(120);
    let tar = Tar::new()
        .pax(&[("path", path.as_bytes())])
        .file(&path, b"x")
        .end();
    let run = extract(&tar);
    run.ok();
    assert!(run.root().join(&path).is_file());
}
