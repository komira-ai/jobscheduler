//! The outputs `mac-farm` fetches from the fake farm: written at their paths below
//! `--download-to` once the command passed, and refused, with nothing written, when the farm
//! serves them other than the command made them.

use super::*;

/// The response of a command that exited 0 and made `files` and `directories`.
fn made(files: Vec<OutputFile>, directories: Vec<OutputDirectory>) -> ExecuteResponse {
    let mut response = exited(0, None, None);
    if let Some(result) = response.result.as_mut() {
        result.output_files = files;
        result.output_directories = directories;
    }
    response
}

fn output_file(path: &str, digest: &Digest, executable: bool) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        digest: Some(digest.clone()),
        is_executable: executable,
        ..OutputFile::default()
    }
}

/// The flags of a run that declares `target/release/js` and `dist` and sets one variable.
fn with_outputs(args: Args) -> Args {
    Args {
        outputs: vec!["target/release/js".to_owned(), "dist".to_owned()],
        download_to: PathBuf::from("fetched"),
        env: vec!["MACOSX_DEPLOYMENT_TARGET=11.0".to_owned()],
        ..args
    }
}

/// Catches outputs the Command does not declare (the farm would keep none), a variable the job
/// asked for left out of its environment, a fetched file written anywhere but at its path below
/// `--download-to`, an executable written without its mode, a directory's nested file or empty
/// directory left out, and a passing run that does not say what it fetched.
#[test]
fn the_outputs_of_a_command_that_passed_are_written_at_their_paths() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::new();
    let js = rig.put(b"mach-o js");
    let sums = rig.put(b"sums\n");
    let nested = rig.put(b"nested");
    let deep = Directory {
        files: vec![FileNode {
            name: "x".to_owned(),
            digest: Some(nested),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let empty = Directory::default();
    let root = Directory {
        files: vec![FileNode {
            name: "SHA256SUMS".to_owned(),
            digest: Some(sums),
            ..FileNode::default()
        }],
        directories: vec![
            DirectoryNode {
                name: "deep".to_owned(),
                digest: Some(digest_of(&deep.encode_to_vec())),
            },
            DirectoryNode {
                name: "empty".to_owned(),
                digest: Some(digest_of(&empty.encode_to_vec())),
            },
        ],
        ..Directory::default()
    };
    let tree = rig.put(
        &Tree {
            root: Some(root),
            children: vec![deep, empty],
        }
        .encode_to_vec(),
    );
    rig.state.queue(Answer::Updates(vec![answered(
        "op/20",
        &made(
            vec![output_file("target/release/js", &js, true)],
            vec![OutputDirectory {
                path: "dist".to_owned(),
                tree_digest: Some(tree),
                ..OutputDirectory::default()
            }],
        ),
    )]));
    let dir = scratch("outputs");
    let (result, _, _) = rig.run_in(&dir, &with_outputs);
    let fetched = dir.path().join("checkout/fetched");
    assert_eq!(
        result.expect("ok"),
        [
            "ok: `cargo build -p js-cli` passed on a farm Mac".to_owned(),
            format!(
                "mac-farm: fetched 3 files (20 bytes) of dist, target/release/js into {}",
                fetched.display()
            ),
        ]
    );
    let (_, command) = rig.executed(0);
    assert_eq!(command.output_paths, ["dist", "target/release/js"]);
    assert!(
        command
            .environment_variables
            .iter()
            .any(|v| v.name == "MACOSX_DEPLOYMENT_TARGET" && v.value == "11.0")
    );
    let read = |p: &str| std::fs::read(fetched.join(p)).expect("an output");
    assert_eq!(read("target/release/js"), b"mach-o js");
    assert_eq!(read("dist/SHA256SUMS"), b"sums\n");
    assert_eq!(read("dist/deep/x"), b"nested");
    assert!(fetched.join("dist/empty").is_dir());
    let mode = std::fs::metadata(fetched.join("target/release/js"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

/// Catches an output the farm serves with other bytes than its digest written anyway (and the
/// other outputs written before the check), a declared output the command did not make read as
/// a pass, an output or a tree the farm cannot serve taken as empty, and the outputs of a command
/// that failed fetched at all.
#[test]
fn outputs_the_farm_cannot_serve_as_made_fail_the_run_and_nothing_is_written() {
    let rig = Rig::new();
    let js = rig.put(b"mach-o js");
    let claimed = digest_of(b"the real sums");
    rig.state
        .blobs
        .lock()
        .expect("blobs")
        .insert(claimed.hash.clone(), b"other bytes".to_vec());
    let empty_tree = rig.put(&Tree::default().encode_to_vec());
    let dir_of = |tree: &Digest| OutputDirectory {
        path: "dist".to_owned(),
        tree_digest: Some(tree.clone()),
        ..OutputDirectory::default()
    };
    let corrupt = made(
        vec![
            output_file("target/release/js", &js, true),
            output_file("dist", &claimed, false),
        ],
        Vec::new(),
    );
    let missing = made(
        vec![output_file("target/release/js", &js, true)],
        Vec::new(),
    );
    let unreadable = made(
        vec![output_file(
            "target/release/js",
            &digest_of(b"never stored"),
            true,
        )],
        vec![dir_of(&empty_tree)],
    );
    let no_tree = made(
        vec![output_file("target/release/js", &js, true)],
        vec![dir_of(&digest_of(b"no such tree"))],
    );
    let mut failed = made(
        vec![output_file("target/release/js", &js, true)],
        Vec::new(),
    );
    if let Some(result) = failed.result.as_mut() {
        result.exit_code = 1;
    }
    for (n, (response, want)) in [
        (
            corrupt,
            format!(
                "the farm served {} as bytes with the SHA-256 {} (11 bytes read)",
                claimed.hash,
                digest_of(b"other bytes").hash
            ),
        ),
        (missing, "the command passed but made no dist".to_owned()),
        (
            unreadable,
            "ByteStream.Read: NotFound: no blobs/".to_owned(),
        ),
        (no_tree, "ByteStream.Read: NotFound: no blobs/".to_owned()),
        (
            failed,
            "`cargo build -p js-cli` exited 1 on a farm Mac".to_owned(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        rig.state.queue(Answer::Updates(vec![answered(
            &format!("op/{n}"),
            &response,
        )]));
        let dir = scratch("outputs-refused");
        let (result, _, _) = rig.run_in(&dir, &with_outputs);
        let got = failure(result);
        assert!(got.starts_with(&want), "{got}");
        assert!(
            !dir.path().join("checkout/fetched").exists(),
            "{want}: something was written"
        );
    }
}

/// Catches a directory of an output that cannot be made here taken as written: the run fails
/// naming it.
#[test]
fn an_output_directory_that_cannot_be_made_fails_the_run() {
    let rig = Rig::new();
    let js = rig.put(b"mach-o js");
    let tree = rig.put(&Tree::default().encode_to_vec());
    rig.state.queue(Answer::Updates(vec![answered(
        "op/31",
        &made(
            vec![output_file("target/release/js", &js, true)],
            vec![OutputDirectory {
                path: "dist".to_owned(),
                tree_digest: Some(tree),
                ..OutputDirectory::default()
            }],
        ),
    )]));
    let dir = scratch("outputs-blocked");
    std::fs::create_dir_all(dir.path().join("checkout")).expect("mkdir");
    std::fs::write(dir.path().join("checkout/fetched"), "a file").expect("write");
    let (result, _, _) = rig.run_in(&dir, &with_outputs);
    let got = failure(result);
    let blocked = dir.path().join("checkout/fetched/dist");
    assert!(
        got.starts_with(&format!("cannot write {}: ", blocked.display())),
        "{got}"
    );
}

/// Catches a blob the farm streams on past its digest's size read to its end (a farm could fill
/// this machine's memory), and bytes that are not the blob taken: the read stops at the first
/// message past the size and fails naming the digest.
#[test]
fn a_blob_longer_than_its_digest_is_cut_short_and_refused() {
    let rig = Rig::new();
    let farm = rig.farm("");
    let digest = digest_of(b"abc");
    let long = vec![7_u8; 1 << 20];
    rig.state
        .blobs
        .lock()
        .expect("blobs")
        .insert(digest.hash.clone(), long);
    let refused = rig
        .runtime
        .block_on(farm.read(&digest))
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the farm served {} as bytes with the SHA-256 {} (6 bytes read)",
            digest.hash,
            digest_of(&[7, 7, 7, 7, 7, 7]).hash
        )
    );
}
