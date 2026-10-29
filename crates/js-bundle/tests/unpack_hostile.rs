//! The extractor against hostile archives: every refusal of the format's hostile-content
//! table, each with the archive that provokes it, and the check that nothing was created
//! outside the target directory.

mod layer;

use js_bundle::{Defect, EntryDefect, Limits, LinkDefect, StreamError, TreeRefusal, UnpackError};
use layer::{Entry, Tar, mode_of, run, run_with, snapshot};

macro_rules! refused {
    ($run:expr, $($pattern:tt)+) => {
        let outcome = &$run;
        assert!(matches!(outcome.err(), $($pattern)+), "got {:?}", outcome.err());
    };
}

fn path_defect(run: &layer::Run) -> Defect {
    match run.err() {
        UnpackError::Tree(TreeRefusal::Path { defect, .. }) => *defect,
        other => panic!("not a path refusal: {other:?}"),
    }
}

// ---- names ----

/// An absolute path never reaches the file system.
#[test]
fn absolute_paths_are_refused() {
    let run = run(&Tar::new().file("/etc/passwd", b"x").end());
    assert_eq!(path_defect(&run), Defect::Absolute);
    run.scratch.assert_contained();
}

/// A `..` component is refused wherever it is.
#[test]
fn dot_dot_components_are_refused() {
    for name in ["../outside/x", "a/../../outside/x", "a/..", ".."] {
        let run = run(&Tar::new().dir("a").file(name, b"x").end());
        assert_eq!(path_defect(&run), Defect::DotComponent, "{name}");
        run.scratch.assert_contained();
    }
}

/// Names outside the allowed bytes are refused: not UTF-8, not ASCII, spaces, control bytes.
#[test]
fn names_outside_the_allowed_bytes_are_refused() {
    let names: [&[u8]; 5] = [
        b"\xff\xfe",
        "caf\u{e9}".as_bytes(),
        b"a b",
        b"a\tb",
        b"a\x7fb",
    ];
    for name in names {
        let run = run(&Tar::new().entry(Entry::new(name, b'0').data(b"x")).end());
        assert_eq!(path_defect(&run), Defect::ForbiddenByte, "{name:?}");
    }
}

/// Names the volume tools treat specially, AppleDouble names, over-long components, paths
/// and depths are refused as entries.
#[test]
fn special_and_over_long_names_are_refused() {
    for name in [
        ".fseventsd",
        ".Spotlight-V100",
        ".Trashes",
        ".metadata_never_index",
    ] {
        let run = run(&Tar::new().dir(name).end());
        assert_eq!(path_defect(&run), Defect::RootReserved, "{name}");
    }
    let run = run(&Tar::new().dir("d").file("d/._x", b"x").end());
    assert_eq!(path_defect(&run), Defect::AppleDouble);

    let component = "a".repeat(256);
    let run = run(&Tar::new().long_file(&component, b"x").end());
    assert_eq!(path_defect(&run), Defect::ComponentTooLong);

    let path = format!("{}/{}/ab", "a".repeat(255), "a".repeat(254));
    let run = run(&Tar::new().long_file(&path, b"x").end());
    assert_eq!(path_defect(&run), Defect::TooLong);

    let deep = vec!["a"; 65].join("/");
    let run = run(&Tar::new().long_file(&deep, b"x").end());
    assert_eq!(path_defect(&run), Defect::TooDeep);
}

/// A name that differs only in ASCII case, and a path that appears twice, are refused.
#[test]
fn duplicate_and_case_colliding_paths_are_refused() {
    let tar = Tar::new().file("a", b"1").file("a", b"2").end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::Duplicate,
            ..
        })
    );
    let tar = Tar::new().dir("a").file("a", b"2").end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::Duplicate,
            ..
        })
    );
    let tar = Tar::new().file("a", b"1").file("A", b"2").end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::CaseCollision,
            ..
        })
    );
    let tar = Tar::new()
        .dir("d")
        .file("d/X", b"1")
        .file("d/x", b"2")
        .end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::CaseCollision,
            ..
        })
    );
}

/// An entry whose parent directory comes later, or is missing, is refused: nothing is
/// created implicitly.
#[test]
fn a_child_before_its_parent_is_refused() {
    let tar = Tar::new().file("d/f", b"x").dir("d").end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::ParentMissing,
            ..
        })
    );
    let tar = Tar::new().dir("a").file("a/b/c", b"x").end();
    refused!(
        run(&tar),
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::ParentMissing,
            ..
        })
    );
}

// ---- symlinks ----

/// A file cannot be written through a symlink an earlier entry of the same archive made.
#[test]
fn a_symlink_made_earlier_in_the_archive_is_not_written_through() {
    let tar = Tar::new()
        .symlink("link", "../outside")
        .file("link/pwned", b"x")
        .end();
    let run = run(&tar);
    refused!(
        run,
        UnpackError::Tree(TreeRefusal::Entry {
            defect: EntryDefect::ParentNotDirectory,
            ..
        })
    );
    run.scratch.assert_contained();
}

/// A symlink that leaves the tree is refused once the stream has ended.
#[test]
fn a_symlink_that_leaves_the_tree_is_refused() {
    for (name, target) in [
        ("link", "../outside"),
        ("d/link", "../.."),
        ("d/up", "../../x"),
    ] {
        let run = run(&Tar::new().dir("d").symlink(name, target).end());
        refused!(
            run,
            UnpackError::Tree(TreeRefusal::Link {
                defect: LinkDefect::Escapes,
                ..
            })
        );
        run.scratch.assert_contained();
    }
}

/// A symlink that stays inside is admitted, `..` included.
#[test]
fn a_symlink_that_stays_inside_is_admitted() {
    let tar = Tar::new()
        .dir("d")
        .file("f", b"x")
        .symlink("d/up", "..")
        .symlink("d/f", "../f")
        .end();
    let run = run(&tar);
    run.ok();
    assert_eq!(
        std::fs::read_link(run.root().join("d/up"))
            .unwrap()
            .to_str(),
        Some("..")
    );
}

/// The chain `d/s -> ..` then `t -> d/s/..` looks inside when read as text; resolved the way
/// the kernel does it, it lands above the tree. The first link alone is fine.
#[test]
fn a_symlink_chain_that_climbs_through_another_symlink_is_refused() {
    let first = Tar::new().dir("d").symlink("d/s", "..");
    run(&first.end()).ok();
    let tar = Tar::new()
        .dir("d")
        .symlink("d/s", "..")
        .symlink("t", "d/s/..")
        .end();
    let run = run(&tar);
    match run.err() {
        UnpackError::Tree(TreeRefusal::Link { path, defect }) => {
            assert_eq!(path, "t");
            assert_eq!(*defect, LinkDefect::Escapes);
        }
        other => panic!("{other:?}"),
    }
}

/// Absolute and over-long targets, empty targets and targets with a NUL are refused.
#[test]
fn bad_symlink_targets_are_refused() {
    let target_defect = |tar: Vec<u8>| match run(&tar).err() {
        UnpackError::Tree(TreeRefusal::Target { defect, .. }) => *defect,
        other => panic!("not a target refusal: {other:?}"),
    };
    assert_eq!(
        target_defect(Tar::new().symlink("abs", "/etc/passwd").end()),
        Defect::Absolute
    );
    assert_eq!(
        target_defect(Tar::new().symlink("empty", "").end()),
        Defect::Empty
    );
    let long = format!("{}/{}", "b".repeat(1000), "c".repeat(25));
    let tar = Tar::new()
        .long_link(long.as_bytes())
        .symlink("long", &long)
        .end();
    assert_eq!(target_defect(tar), Defect::TooLong);
    let mut nul = vec![b'a'; 120];
    nul.push(0);
    nul.push(b'b');
    let tar = Tar::new()
        .long_link(&nul)
        .entry(Entry::new(b"nul", b'2').link(&nul))
        .end();
    assert_eq!(target_defect(tar), Defect::ForbiddenByte);
}

/// The walker: a dangling link is fine while the part that exists is inside and the rest
/// is plain names; climbing from a missing entry, going through a file, loops and more than
/// eight hops are refused.
#[test]
fn the_walker_follows_the_kernel_through_dangling_files_and_loops() {
    run(&Tar::new().symlink("a", "missing/x").end()).ok();
    run(&Tar::new().file("f", b"x").symlink("s", "f").end()).ok();
    run(&Tar::new().symlink("here", ".").end()).ok();

    let link_defect = |tar: Vec<u8>| match run(&tar).err() {
        UnpackError::Tree(TreeRefusal::Link { defect, .. }) => *defect,
        other => panic!("not a link refusal: {other:?}"),
    };
    assert_eq!(
        link_defect(Tar::new().symlink("a", "missing/..").end()),
        LinkDefect::Dangling
    );
    assert_eq!(
        link_defect(Tar::new().file("f", b"x").symlink("s", "f/x").end()),
        LinkDefect::NotDirectory
    );
    let tar = Tar::new().symlink("a", "b").symlink("b", "a").end();
    assert_eq!(link_defect(tar), LinkDefect::TooManyHops);

    // l0 -> l1 -> ... -> l8 -> f: eight hops pass, nine do not.
    let chain = |hops: usize| {
        let mut tar = Tar::new().file("f", b"x");
        for i in 0..hops {
            let next = if i + 1 == hops {
                "f".to_string()
            } else {
                format!("l{}", i + 1)
            };
            tar = tar.symlink(&format!("l{i}"), &next);
        }
        tar.end()
    };
    run(&chain(9)).ok();
    assert_eq!(link_defect(chain(10)), LinkDefect::TooManyHops);
}

// ---- entry types and modes ----

/// Hard links are refused, whatever they point at.
#[test]
fn hardlinks_are_refused() {
    for target in ["f", "../outside/x"] {
        let tar = Tar::new()
            .file("f", b"x")
            .entry(Entry::new(b"h", b'1').link(target.as_bytes()))
            .end();
        refused!(run(&tar), UnpackError::EntryType { kind: '1', .. });
    }
}

/// Devices, fifos, contiguous files, global PAX headers, sparse files and any type this
/// format has never heard of (sockets have none in tar) are refused.
#[test]
fn devices_fifos_sockets_and_unknown_types_are_refused() {
    for (flag, kind) in [
        (b'3', '3'),
        (b'4', '4'),
        (b'6', '6'),
        (b'7', '7'),
        (b'g', 'g'),
        (b'S', 'S'),
        (b's', 's'),
    ] {
        let run = run(&Tar::new().entry(Entry::new(b"n", flag)).end());
        match run.err() {
            UnpackError::EntryType { kind: got, .. } => assert_eq!(*got, kind),
            other => panic!("{other:?}"),
        }
        assert!(snapshot(&run.root()).is_empty());
    }
}

/// Setuid, setgid and sticky bits in a header never reach the disk: modes are rebuilt from
/// the execute bit alone.
#[test]
fn set_id_and_sticky_bits_never_reach_the_disk() {
    let tar = Tar::new()
        .entry(Entry::new(b"suid", b'0').mode(0o4755).data(b"x"))
        .entry(Entry::new(b"sgid", b'0').mode(0o2755).data(b"x"))
        .entry(Entry::new(b"sticky", b'0').mode(0o1644).data(b"x"))
        .entry(Entry::new(b"plain", b'0').mode(0o600).data(b"x"))
        .entry(Entry::new(b"other-x", b'0').mode(0o001).data(b"x"))
        .entry(Entry::new(b"dir", b'5').mode(0o4777))
        .end();
    let run = run(&tar);
    run.ok();
    for (name, mode) in [
        ("suid", 0o755),
        ("sgid", 0o755),
        ("sticky", 0o644),
        ("plain", 0o644),
        ("other-x", 0o755),
        ("dir", 0o755),
    ] {
        assert_eq!(mode_of(&run.root().join(name)), mode, "{name}");
    }
}

/// A directory or a symlink that declares data is refused.
#[test]
fn a_directory_or_symlink_with_a_size_is_refused() {
    let tar = Tar::new().entry(Entry::new(b"d", b'5').data(b"junk")).end();
    refused!(run(&tar), UnpackError::SizeOnNonFile { .. });
    let tar = Tar::new()
        .entry(Entry::new(b"l", b'2').link(b"x").data(b"junk"))
        .end();
    refused!(run(&tar), UnpackError::SizeOnNonFile { .. });
}

// ---- limits ----

/// An entry that declares more bytes than the layer may hold is refused before a byte of
/// it is read, and so is a total over the limit.
#[test]
fn oversize_entries_and_totals_are_refused() {
    let limits = Limits {
        entries: 10,
        bytes: 1 << 20,
    };
    let tar = Tar::new()
        .entry(Entry::new(b"big", b'0').size(0o77777777777))
        .end();
    let run = run_with(&tar, &limits);
    refused!(run, UnpackError::TooManyBytes { max } if *max == 1 << 20);
    assert!(snapshot(&run.root()).is_empty());

    let chunk = vec![7u8; 400 << 10];
    let tar = Tar::new()
        .file("a", &chunk)
        .file("b", &chunk)
        .file("c", &chunk)
        .end();
    let run = run_with(&tar, &limits);
    refused!(run, UnpackError::TooManyBytes { .. });
    assert_eq!(snapshot(&run.root()).len(), 2);
}

/// More entries than the limit are refused; the limit itself is allowed.
#[test]
fn too_many_entries_are_refused() {
    let limits = Limits {
        entries: 3,
        bytes: 1 << 20,
    };
    run_with(&Tar::new().dir("a").dir("b").dir("c").end(), &limits).ok();
    let tar = Tar::new().dir("a").dir("b").dir("c").dir("d").end();
    refused!(
        run_with(&tar, &limits),
        UnpackError::TooManyEntries { max: 3 }
    );
}

/// Bytes after the end marker are read, hashed and counted against the ceiling: within it
/// they are part of the diff id, beyond it the stream is refused.
#[test]
fn trailing_bytes_after_the_end_marker_are_bounded_by_the_raw_ceiling() {
    let body = Tar::new().dir("a").file("a/f", b"hello").body();
    let tight = Limits {
        entries: 10,
        bytes: body.len() as u64,
    };
    let mut exact = body.clone();
    exact.resize(body.len() + 1024, 0);
    let run_exact = run_with(&exact, &tight);
    assert_eq!(run_exact.ok().stream_bytes, exact.len() as u64);
    assert_eq!(run_exact.ok().diff_id, layer::diff_id_of(&exact));

    let mut over = exact.clone();
    over.push(0);
    let run_over = run_with(&over, &tight);
    refused!(run_over, UnpackError::Stream(StreamError::Ceiling { max }) if *max == body.len() as u64 + 1024);

    let roomy = Limits {
        entries: 10,
        bytes: body.len() as u64 + 4096,
    };
    let mut padded = exact;
    padded.resize(padded.len() + 3000, 0);
    let run_padded = run_with(&padded, &roomy);
    assert_eq!(run_padded.ok().stream_bytes, padded.len() as u64);
    assert_eq!(run_padded.ok().diff_id, layer::diff_id_of(&padded));
}

// ---- records ----

/// A PAX path that differs from the header's, a long name that does not extend the header's,
/// and a PAX link path that differs, are refused.
#[test]
fn a_header_and_its_records_must_agree() {
    let tar = Tar::new()
        .dir("dir")
        .pax(&[("path", b"dir/a")])
        .entry(Entry::new(b"dir/b", b'0').data(b"x"))
        .end();
    refused!(run(&tar), UnpackError::Disagree { .. });

    let long = format!("{}/f", "a".repeat(150));
    let tar = Tar::new()
        .long_name(long.as_bytes())
        .entry(Entry::new(b"zzz", b'0').data(b"x"))
        .end();
    refused!(run(&tar), UnpackError::Disagree { .. });

    let tar = Tar::new()
        .pax(&[("linkpath", b"y")])
        .entry(Entry::new(b"l", b'2').link(b"x"))
        .end();
    refused!(run(&tar), UnpackError::Disagree { .. });

    let tar = Tar::new()
        .long_name(long.as_bytes())
        .pax(&[("path", b"other")])
        .entry(Entry::new(long.as_bytes(), b'0').data(b"x"))
        .end();
    refused!(run(&tar), UnpackError::Disagree { .. });

    let tar = Tar::new()
        .pax(&[("linkpath", b"y")])
        .entry(Entry::new(b"f", b'0').data(b"x"))
        .end();
    refused!(run(&tar), UnpackError::Disagree { .. });
}

/// The path that counts is the long one: a `..` in a PAX path whose header holds only the
/// first 100 bytes is caught.
#[test]
fn a_pax_path_is_the_effective_path() {
    let path = format!("{}/../x", "a".repeat(99));
    let tar = Tar::new()
        .pax(&[("path", path.as_bytes())])
        .entry(Entry::new(path.as_bytes(), b'0').data(b"x"))
        .end();
    assert_eq!(path_defect(&run(&tar)), Defect::DotComponent);
}

/// Extended attributes, sparse maps, ACLs and size overrides in a PAX record are refused, as
/// is the same key twice.
#[test]
fn pax_records_that_carry_more_than_a_name_are_refused() {
    for key in [
        "SCHILY.xattr.user.k",
        "GNU.sparse.map",
        "LIBARCHIVE.creationtime",
        "SCHILY.acl.access",
        "size",
    ] {
        let tar = Tar::new().pax(&[(key, b"1")]).file("f", b"x").end();
        match run(&tar).err() {
            UnpackError::Extension { key: got, .. } => assert_eq!(got, key),
            other => panic!("{other:?}"),
        }
    }
    let tar = Tar::new()
        .pax(&[("path", b"f"), ("path", b"f")])
        .file("f", b"x")
        .end();
    refused!(run(&tar), UnpackError::Extension { .. });
}

/// The harmless PAX keys are accepted and ignored.
#[test]
fn harmless_pax_records_are_accepted() {
    let tar = Tar::new()
        .pax(&[
            ("mtime", b"1.5"),
            ("uid", b"0"),
            ("gid", b"0"),
            ("uname", b"x"),
            ("path", b"f"),
        ])
        .file("f", b"x")
        .end();
    run(&tar).ok();
}

/// A record over 8 KiB, two records of one kind for one entry, and a record at the end of
/// the archive that describes nothing, are refused; a long name without its NUL is read as is.
#[test]
fn malformed_record_sequences_are_refused() {
    let big = vec![b'a'; 8193];
    refused!(
        run(&Tar::new().record(b'x', &big).file("f", b"x").end()),
        UnpackError::Record { .. }
    );
    let tar = Tar::new()
        .long_name(b"f")
        .long_name(b"f")
        .file("f", b"x")
        .end();
    refused!(run(&tar), UnpackError::Record { .. });
    let tar = Tar::new().file("f", b"x").long_name(b"g").end();
    refused!(run(&tar), UnpackError::Record { .. });
    let tar = Tar::new().record(b'L', b"f").file("f", b"x").end();
    run(&tar).ok();
}

// ---- the stream ----

/// A malformed PAX record is a tar error.
#[test]
fn a_malformed_pax_record_is_refused() {
    let tar = Tar::new()
        .record(b'x', b"not a record\n")
        .file("f", b"x")
        .end();
    refused!(run(&tar), UnpackError::Tar(_));
}

/// Anything but a zero block after the first end block is refused.
#[test]
fn a_second_end_block_that_is_not_zero_is_refused() {
    let mut tar = Tar::new().file("f", b"x").body();
    tar.resize(tar.len() + 512, 0);
    tar.extend_from_slice(&[1u8; 512]);
    refused!(run(&tar), UnpackError::EndMarker);
}

/// A stream cut anywhere is refused as truncated: inside a header, inside data, between
/// entries, with no end marker, or with half of it.
#[test]
fn a_truncated_archive_is_refused_wherever_it_is_cut() {
    let tar = Tar::new()
        .dir("a")
        .file("a/f", b"hello")
        .file("a/g", &[9u8; 700])
        .end();
    let mut cuts: Vec<usize> = (0..tar.len()).step_by(37).collect();
    cuts.extend([
        0,
        1,
        511,
        512,
        513,
        1024,
        1029,
        1536,
        tar.len() - 1024,
        tar.len() - 513,
        tar.len() - 512,
        tar.len() - 511,
        tar.len() - 1,
    ]);
    for cut in cuts {
        let run = run(&tar[..cut]);
        refused!(run, UnpackError::Stream(StreamError::Truncated));
    }
    run(&tar).ok();
}

/// A reader that fails is a refusal, not a panic.
#[test]
fn a_reader_that_fails_is_refused() {
    struct Failing;
    impl std::io::Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }
    use std::os::fd::AsFd as _;
    let scratch = layer::Scratch::new();
    let fd = scratch.root_fd();
    match js_bundle::unpack_tar(Failing, fd.as_fd(), &layer::limits()) {
        Err(UnpackError::Tar(why)) => assert!(why.contains("disk on fire"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Extracting into a directory that already holds the first entry fails; it never
/// overwrites.
#[test]
fn an_existing_path_is_never_overwritten() {
    let scratch = layer::Scratch::new();
    std::fs::write(scratch.root().join("f"), b"mine").unwrap();
    let fd = scratch.root_fd();
    use std::os::fd::AsFd as _;
    let tar = Tar::new().file("f", b"theirs").end();
    match js_bundle::unpack_tar(&tar[..], fd.as_fd(), &layer::limits()) {
        Err(UnpackError::Create { path, .. }) => assert_eq!(path, "f"),
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(scratch.root().join("f")).unwrap(), b"mine");
}
