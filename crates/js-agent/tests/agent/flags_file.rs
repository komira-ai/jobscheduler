//! `--flags-file`: the file's words stand where the flag stood, and a file that breaks a
//! rule is refused with a message that names it.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::Path;

use js_agent::flags_file::{
    FLAG, FileFacts, FlagsFileError, MAX_BYTES, expand, expand_with, read_system, words,
};

use crate::support::{
    CountingSleeper, Lowest, Scratch, Script, ScriptConnector, code, linux_host, run, welcome,
};

fn os(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn texts(list: Vec<OsString>) -> Vec<String> {
    list.into_iter()
        .map(|word| word.into_string().unwrap())
        .collect()
}

/// A file as the reader seam gives it: regular, mode 0644, owned by user 1000.
fn facts(text: &str) -> io::Result<FileFacts> {
    Ok(FileFacts {
        regular: true,
        mode: 0o644,
        uid: 1000,
        bytes: text.as_bytes().to_vec(),
    })
}

fn expanded(args: &[&str], reader: &dyn Fn(&Path) -> io::Result<FileFacts>) -> Vec<String> {
    texts(expand_with(os(args), 1000, reader).unwrap())
}

fn refused(args: &[&str], reader: &dyn Fn(&Path) -> io::Result<FileFacts>) -> String {
    expand_with(os(args), 1000, reader).unwrap_err().to_string()
}

/// The pair is replaced in place by the file's words, so what follows it on the command
/// line is read after them, and the program name stays first. Catches an expansion that
/// appends the words or drops what followed.
#[test]
fn the_pair_is_replaced_by_the_words_of_the_file_in_place() {
    let got = expanded(
        &["js-agent", "--a", "--flags-file", "/f", "--z=1"],
        &|path| {
            assert_eq!(path, Path::new("/f"));
            facts("--server=https://leases.example:7300\n--pool\n")
        },
    );
    assert_eq!(
        got,
        [
            "js-agent",
            "--a",
            "--server=https://leases.example:7300",
            "--pool",
            "--z=1"
        ]
    );
}

/// The attached spelling works the same, including for a path that is not UTF-8.
#[test]
fn the_attached_spelling_is_replaced_too() {
    let got = expanded(&["js-agent", "--flags-file=/f", "--x"], &|path| {
        assert_eq!(path, Path::new("/f"));
        facts("--pool=a")
    });
    assert_eq!(got, ["js-agent", "--pool=a", "--x"]);
    let odd = vec![
        OsString::from("js-agent"),
        OsString::from_vec(b"--flags-file=/\xff".to_vec()),
    ];
    let seen = expand_with(odd, 1000, &|path| {
        assert_eq!(path.as_os_str(), OsString::from_vec(b"/\xff".to_vec()));
        facts("")
    })
    .unwrap();
    assert_eq!(seen, os(&["js-agent"]));
}

/// Arguments with no flags file pass through untouched, and an empty argument list is fine.
#[test]
fn arguments_without_the_flag_are_unchanged() {
    let never = |_: &Path| -> io::Result<FileFacts> { panic!("read") };
    assert_eq!(
        expanded(&["js-agent", "--flags-filex", "--pool=a", "x"], &never),
        ["js-agent", "--flags-filex", "--pool=a", "x"]
    );
    assert!(expand_with(Vec::new(), 1000, &never).unwrap().is_empty());
}

/// Whitespace is trimmed, and empty lines and comment lines are skipped.
#[test]
fn words_trims_and_skips_blank_and_comment_lines() {
    let got = words("# the server\n  --server=https://x.example  \n\n\t\n   # more\n--pool=p\r\n")
        .unwrap();
    assert_eq!(got, ["--server=https://x.example", "--pool=p"]);
    assert!(words("").unwrap().is_empty());
}

/// A line that is not a flag is refused with its number, and so is a bare `--`.
#[test]
fn words_refuses_a_line_that_does_not_start_with_two_dashes() {
    for (text, line) in [
        ("--a\nserver\n", 2),
        ("-s\n", 1),
        ("--\n", 1),
        ("\n# c\nx", 3),
    ] {
        let why = words(text).unwrap_err();
        assert!(
            matches!(why, FlagsFileError::NotAFlag { line: l } if l == line),
            "{why}"
        );
    }
    assert_eq!(
        words("a").unwrap_err().to_string(),
        "line 1: not a flag (every line must start with --)"
    );
}

/// A flags file may not name a flags file, in either spelling; a flag that only starts
/// with the name is another flag.
#[test]
fn words_refuses_a_nested_flags_file() {
    for text in [
        "--flags-file\n",
        "--flags-file=/x\n",
        "--a\n  --flags-file=/x",
    ] {
        let why = words(text).unwrap_err();
        assert!(
            matches!(why, FlagsFileError::Nested { .. }),
            "{text}: {why}"
        );
    }
    assert_eq!(
        words("--a\n--flags-file").unwrap_err().to_string(),
        "line 2: --flags-file may not appear in a flags file"
    );
    assert_eq!(words("--flags-files=1\n").unwrap(), ["--flags-files=1"]);
}

/// The flag given twice (any mix of spellings) is refused, and a flag without its path is.
#[test]
fn the_flag_twice_or_without_a_path_is_refused() {
    let one = |_: &Path| facts("--pool=a");
    assert_eq!(
        refused(&["p", "--flags-file", "/a", "--flags-file", "/b"], &one),
        "--flags-file: given more than once"
    );
    assert_eq!(
        refused(&["p", "--flags-file=/a", "--flags-file=/b"], &one),
        "--flags-file: given more than once"
    );
    assert_eq!(
        refused(&["p", "--flags-file"], &one),
        "--flags-file: needs a path"
    );
}

/// Each rule on the file's facts refuses with a message that names the file and why.
#[test]
fn a_file_that_breaks_a_rule_is_refused_naming_it() {
    let args = ["p", "--flags-file", "/etc/js-agent/flags"];
    let with = |change: fn(&mut FileFacts)| {
        refused(&args, &move |_: &Path| {
            let mut file = facts("--pool=a")?;
            change(&mut file);
            Ok(file)
        })
    };
    assert_eq!(
        with(|f| f.regular = false),
        "--flags-file /etc/js-agent/flags: not a regular file"
    );
    assert_eq!(
        with(|f| f.mode = 0o664),
        "--flags-file /etc/js-agent/flags: writable by group or others (mode 0664)"
    );
    assert_eq!(
        with(|f| f.mode = 0o602),
        "--flags-file /etc/js-agent/flags: writable by group or others (mode 0602)"
    );
    assert_eq!(
        with(|f| f.uid = 1001),
        "--flags-file /etc/js-agent/flags: owned by user 1001, not by the user running the agent or root"
    );
    assert_eq!(
        with(|f| f.bytes = vec![b'#'; usize::try_from(MAX_BYTES).unwrap() + 1]),
        "--flags-file /etc/js-agent/flags: larger than 65536 bytes"
    );
    assert_eq!(
        with(|f| f.bytes = vec![b'-', b'-', 0xff]),
        "--flags-file /etc/js-agent/flags: not UTF-8 text"
    );
    assert_eq!(
        with(|f| f.bytes = b"--a\nb".to_vec()),
        "--flags-file /etc/js-agent/flags: line 2: not a flag (every line must start with --)"
    );
    assert_eq!(
        with(|f| f.bytes = b"--flags-file=/x".to_vec()),
        "--flags-file /etc/js-agent/flags: line 1: --flags-file may not appear in a flags file"
    );
}

/// A file of root, of the user running the agent, with mode 0400 or 0600, or of exactly the
/// largest size, is accepted.
#[test]
fn root_the_agents_user_and_the_size_cap_are_accepted() {
    let args = ["p", "--flags-file", "/f"];
    for (uid, mode) in [(0, 0o644), (1000, 0o600), (1000, 0o400)] {
        let got = expanded(&args, &move |_| {
            Ok(FileFacts {
                regular: true,
                mode,
                uid,
                bytes: b"--a".to_vec(),
            })
        });
        assert_eq!(got, ["p", "--a"]);
    }
    let full = vec![b'#'; usize::try_from(MAX_BYTES).unwrap()];
    assert_eq!(
        expanded(&args, &move |_| Ok(FileFacts {
            regular: true,
            mode: 0o600,
            uid: 0,
            bytes: full.clone()
        })),
        ["p"]
    );
}

/// A file that cannot be read is refused with the system's reason.
#[test]
fn an_unreadable_file_is_refused_with_the_reason() {
    let why = refused(&["p", "--flags-file", "/f"], &|_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    });
    assert!(
        why.starts_with("--flags-file /f: cannot be read: "),
        "{why}"
    );
    let error = expand_with(os(&["p", "--flags-file=/f"]), 0, &|_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert!(matches!(error, FlagsFileError::Unreadable { .. }));
    assert!(std::error::Error::source(&error).is_some());
}

/// The system reader reports a real file's content, mode and owner, follows a link, and
/// stops reading one byte past the cap.
#[test]
fn the_system_reader_reads_files_links_and_stops_past_the_cap() {
    let scratch = Scratch::new("flags-file-system");
    let file = scratch.write("flags", "--pool=a\n", 0o600);
    let got = read_system(&file).unwrap();
    assert!(got.regular);
    assert_eq!(got.mode, 0o600);
    assert_eq!(got.bytes, b"--pool=a\n");
    assert_eq!(got.uid, rustix::process::geteuid().as_raw());
    let link = scratch.path("link");
    symlink(&file, &link).unwrap();
    assert_eq!(read_system(&link).unwrap(), got);
    let big = scratch.write(
        "big",
        &"#".repeat(usize::try_from(MAX_BYTES).unwrap() + 10),
        0o600,
    );
    assert_eq!(read_system(&big).unwrap().bytes.len() as u64, MAX_BYTES + 1);
}

/// A directory is not a regular file and its content is not read; a missing file is an
/// error of the open.
#[test]
fn the_system_reader_refuses_a_directory_and_a_missing_file() {
    let scratch = Scratch::new("flags-file-dir");
    let dir = read_system(&scratch.path("")).unwrap();
    assert!(!dir.regular && dir.bytes.is_empty());
    assert!(read_system(&scratch.path("missing")).is_err());
}

/// `expand` reads the real file as the user running the test, refuses one that others may
/// write, and refuses a directory.
#[test]
fn expand_reads_the_system_file_and_applies_the_rules() {
    let scratch = Scratch::new("flags-file-expand");
    let good = scratch.write("good", "--pool=a\n", 0o640);
    let args = |path: &Path| vec![OsString::from("p"), OsString::from(FLAG), path.into()];
    assert_eq!(expand(args(&good)).unwrap(), os(&["p", "--pool=a"]));
    let loose = scratch.write("loose", "--pool=a\n", 0o664);
    assert!(matches!(
        expand(args(&loose)).unwrap_err(),
        FlagsFileError::Writable { .. }
    ));
    assert!(matches!(
        expand(args(&scratch.path(""))).unwrap_err(),
        FlagsFileError::NotRegular { .. }
    ));
}

/// The words reach clap: `--server` and `--pool` that come only from the file land in the
/// agent's hello, and the flag itself is not an unknown flag. A file that breaks a rule
/// ends the program with the usage status and its message on stderr, before anything is
/// sent.
#[test]
fn the_agent_takes_server_and_pool_from_the_file() {
    let scratch = Scratch::new("flags-file-agent");
    let file = scratch.write(
        "flags",
        "# this host\n--server=http://127.0.0.1:1\n--pool=from-file\n",
        0o600,
    );
    let path = file.to_str().unwrap();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run(
        &["--flags-file", path, "--token-file", "/nonexistent/token"],
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["pool"], "from-file");

    let loose = scratch.write("loose", "--pool=x\n", 0o666);
    let (script, sent) = Script::new(vec![welcome(1)]);
    let ran = run(
        &["--flags-file", loose.to_str().unwrap()],
        &linux_host(),
        &ScriptConnector::of(script),
        &mut CountingSleeper::stopping_at(1),
        &mut Lowest,
    );
    assert_eq!(ran.status(), code(2));
    assert_eq!(
        ran.err,
        format!(
            "js-agent: --flags-file {}: writable by group or others (mode 0666)\n",
            loose.display()
        )
    );
    assert!(sent.borrow().is_empty());
}
