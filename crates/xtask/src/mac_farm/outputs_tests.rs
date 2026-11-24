//! The outputs of a result held to the declared paths, the trees of output directories, and the
//! files written from them.

use std::os::unix::fs::PermissionsExt as _;

use js_testkit::dir::TestDir;

use super::super::proto::reapi::{
    DirectoryNode, FileNode, OutputDirectory, OutputFile, OutputSymlink, SymlinkNode,
};
use super::*;

fn declared(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

fn file(path: &str, bytes: &[u8], executable: bool) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        digest: Some(digest_of(bytes)),
        is_executable: executable,
        ..OutputFile::default()
    }
}

fn want(path: &str, bytes: &[u8], executable: bool) -> Want {
    Want {
        path: path.to_owned(),
        digest: digest_of(bytes),
        executable,
    }
}

fn node(name: &str, bytes: &[u8], executable: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest_of(bytes)),
        is_executable: executable,
        ..FileNode::default()
    }
}

fn sub(name: &str, dir: &Directory) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(digest_of(&dir.encode_to_vec())),
    }
}

/// Catches a declared file or directory left out of what is fetched, an executable bit lost,
/// and a directory's tree digest taken as a file's.
#[test]
fn every_declared_output_is_planned_with_its_digest() {
    let tree = digest_of(b"tree");
    let result = ActionResult {
        output_files: vec![file("bin/js", b"js", true), file("SUMS", b"s", false)],
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(tree.clone()),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    };
    let got = plan(&declared(&["SUMS", "bin/js", "dist"]), &result).expect("a plan");
    assert_eq!(
        got,
        Plan {
            files: vec![want("bin/js", b"js", true), want("SUMS", b"s", false)],
            directories: vec![("dist".to_owned(), tree)],
        }
    );
    assert_eq!(
        plan(&BTreeSet::new(), &ActionResult::default()).expect("nothing"),
        Plan::default()
    );
}

/// Catches a result read as complete when the command did not make a declared output, an output
/// the farm adds that nobody asked for (it would be written here), one answered twice, an output
/// with no digest, and a symlink in any of REAPI's three lists of them (it could point a later
/// write anywhere).
#[test]
fn a_result_that_is_not_the_declared_outputs_is_refused() {
    let only = |result: ActionResult, paths: &[&str]| {
        plan(&declared(paths), &result)
            .expect_err("refused")
            .to_string()
    };
    let one = ActionResult {
        output_files: vec![file("a", b"a", false)],
        ..ActionResult::default()
    };
    assert_eq!(
        only(one.clone(), &["a", "b"]),
        "the command passed but made no b"
    );
    assert_eq!(
        only(one.clone(), &[]),
        "the farm answered with the output a, which the command did not declare"
    );
    let twice = ActionResult {
        output_files: vec![file("a", b"a", false), file("a", b"a", false)],
        ..ActionResult::default()
    };
    assert_eq!(
        only(twice, &["a"]),
        "the farm answered with the output a, which the command did not declare"
    );
    let as_both = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "a".to_owned(),
            tree_digest: Some(digest_of(b"t")),
            ..OutputDirectory::default()
        }],
        ..one
    };
    assert_eq!(
        only(as_both, &["a"]),
        "the farm answered with the output a, which the command did not declare"
    );
    let no_digest = ActionResult {
        output_files: vec![OutputFile {
            path: "a".to_owned(),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    assert_eq!(
        only(no_digest, &["a"]),
        "the farm answered with the output a and no digest"
    );
    let dir_no_digest = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "d".to_owned(),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    };
    assert_eq!(
        only(dir_no_digest, &["d"]),
        "the farm answered with the output d and no digest"
    );
    let link = |path: &str| OutputSymlink {
        path: path.to_owned(),
        target: "/etc".to_owned(),
        ..OutputSymlink::default()
    };
    #[allow(
        deprecated,
        reason = "an older farm answers with the deprecated symlink lists"
    )]
    for result in [
        ActionResult {
            output_symlinks: vec![link("l")],
            ..ActionResult::default()
        },
        ActionResult {
            output_file_symlinks: vec![link("l")],
            ..ActionResult::default()
        },
        ActionResult {
            output_directory_symlinks: vec![link("l")],
            ..ActionResult::default()
        },
    ] {
        assert_eq!(
            only(result, &["l"]),
            "the output l is a symlink; only files and directories are fetched"
        );
    }
}

/// Catches a file of a nested directory put at the wrong path or without its executable bit,
/// an empty directory not made, the children of a tree matched by position instead of by digest,
/// and the files not in path order.
#[test]
fn a_directory_tree_is_its_files_and_directories_at_their_paths() {
    let empty = Directory::default();
    let deep = Directory {
        files: vec![node("x", b"x", false)],
        ..Directory::default()
    };
    let lib = Directory {
        files: vec![node("run", b"#!", true)],
        directories: vec![sub("deep", &deep)],
        ..Directory::default()
    };
    let root = Directory {
        files: vec![node("SUMS", b"s", false)],
        directories: vec![sub("empty", &empty), sub("lib", &lib)],
        ..Directory::default()
    };
    let tree = Tree {
        root: Some(root),
        children: vec![deep, empty, lib],
    };
    let (files, dirs) = walk("out", &tree.encode_to_vec()).expect("a tree");
    assert_eq!(
        files,
        [
            want("out/SUMS", b"s", false),
            want("out/lib/deep/x", b"x", false),
            want("out/lib/run", b"#!", true),
        ]
    );
    assert_eq!(dirs, ["out", "out/empty", "out/lib", "out/lib/deep"]);
    let (files, dirs) = walk("none", &Tree::default().encode_to_vec()).expect("an empty tree");
    assert!(files.is_empty());
    assert_eq!(dirs, ["none"]);
}

/// Catches a tree that does not decode, or names a directory it does not carry, read as empty;
/// a name that would climb out of the output directory or name a path; a symlink; and a file
/// with no digest.
#[test]
fn a_tree_that_does_not_hold_together_is_refused() {
    let refused = |tree: &[u8]| walk("out", tree).expect_err("refused").to_string();
    assert!(
        refused(&[0xff, 0xff]).starts_with("the tree of the output out does not hold together: "),
        "{}",
        refused(&[0xff, 0xff])
    );
    let lost = Tree {
        root: Some(Directory {
            directories: vec![sub("lib", &Directory::default())],
            ..Directory::default()
        }),
        children: Vec::new(),
    };
    assert_eq!(
        refused(&lost.encode_to_vec()),
        "the tree of the output out does not hold together: out/lib is not among its directories"
    );
    let no_digest = Tree {
        root: Some(Directory {
            directories: vec![DirectoryNode {
                name: "lib".to_owned(),
                digest: None,
            }],
            ..Directory::default()
        }),
        children: vec![Directory::default()],
    };
    assert_eq!(
        refused(&no_digest.encode_to_vec()),
        "the tree of the output out does not hold together: out/lib is not among its directories"
    );
    for name in ["", ".", "..", "a/b"] {
        let bad = Tree {
            root: Some(Directory {
                files: vec![node(name, b"x", false)],
                ..Directory::default()
            }),
            children: Vec::new(),
        };
        assert_eq!(
            refused(&bad.encode_to_vec()),
            format!("the output directory out names {name:?}, which is not a plain name")
        );
        let bad_dir = Tree {
            root: Some(Directory {
                directories: vec![sub(name, &Directory::default())],
                ..Directory::default()
            }),
            children: vec![Directory::default()],
        };
        assert_eq!(
            refused(&bad_dir.encode_to_vec()),
            format!("the output directory out names {name:?}, which is not a plain name")
        );
    }
    let link = Tree {
        root: Some(Directory {
            symlinks: vec![SymlinkNode {
                name: "l".to_owned(),
                target: "/etc".to_owned(),
                ..SymlinkNode::default()
            }],
            ..Directory::default()
        }),
        children: Vec::new(),
    };
    assert_eq!(
        refused(&link.encode_to_vec()),
        "the output out/l is a symlink; only files and directories are fetched"
    );
    let no_file_digest = Tree {
        root: Some(Directory {
            files: vec![FileNode {
                name: "f".to_owned(),
                ..FileNode::default()
            }],
            ..Directory::default()
        }),
        children: Vec::new(),
    };
    assert_eq!(
        refused(&no_file_digest.encode_to_vec()),
        "the farm answered with the output out/f and no digest"
    );
}

/// Catches a name given twice in one directory (the second file would silently replace the
/// first), and a tree whose directories name one child over and over taken however many files it
/// stands for.
#[test]
fn a_tree_with_a_name_twice_or_too_many_entries_is_refused() {
    let refused = |tree: &Tree| {
        walk("out", &tree.encode_to_vec())
            .expect_err("refused")
            .to_string()
    };
    let twice = Tree {
        root: Some(Directory {
            files: vec![node("a", b"1", false)],
            directories: vec![sub("a", &Directory::default())],
            ..Directory::default()
        }),
        children: vec![Directory::default()],
    };
    assert_eq!(
        refused(&twice),
        "the output directory out names \"a\" twice"
    );
    // Each level names the level below twice: 2^17 directories from 18 small messages.
    let mut level = Directory::default();
    let mut children = Vec::new();
    for _ in 0..17 {
        children.push(level.clone());
        level = Directory {
            directories: vec![sub("l", &level), sub("r", &level)],
            ..Directory::default()
        };
    }
    let wide = Tree {
        root: Some(level),
        children,
    };
    assert_eq!(
        refused(&wide),
        format!("the output directory out holds more than {MAX_ENTRIES} files and directories")
    );
}

/// Catches a file written without its directories made, with another mode than its kind's
/// (an executable that cannot run, or a file every user may write), and a file that cannot be
/// written taken as written.
#[test]
fn a_file_is_written_at_its_path_with_its_mode() {
    let dir = TestDir::under(std::env::temp_dir(), "xtask-mac-farm-write");
    write(dir.path(), "target/release/js", b"js", true).expect("written");
    write(dir.path(), "dist/SUMS", b"s", false).expect("written");
    let js = dir.path().join("target/release/js");
    assert_eq!(std::fs::read(&js).expect("read"), b"js");
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&js), 0o755);
    assert_eq!(mode(&dir.path().join("dist/SUMS")), 0o644);
    std::fs::write(dir.path().join("plain"), "").expect("write");
    let refused = write(dir.path(), "plain/inside", b"x", false).expect_err("refused");
    assert!(
        refused.to_string().starts_with(&format!(
            "cannot write {}",
            dir.path().join("plain/inside").display()
        )),
        "{refused}"
    );
}
