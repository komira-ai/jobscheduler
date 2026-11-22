//! The start file of `__mac-job-start`.

use super::StartFile;

fn file() -> StartFile {
    StartFile {
        uid: 710,
        gid: 20,
        new_session: true,
        asid_file: "/out/asid".to_owned(),
        profile: "(version 1)".to_owned(),
        params: vec![("RUN".to_owned(), "/r/j".to_owned())],
        program: "/bin/sh".to_owned(),
        args: vec!["build.sh".to_owned()],
    }
}

#[test]
fn a_start_file_reads_back_as_it_was_written() {
    let text = serde_json::to_string(&file()).expect("json");
    assert_eq!(StartFile::parse(&text), Ok(file()));
}

#[test]
fn a_nul_anywhere_or_an_unknown_field_is_refused() {
    let with = |change: fn(&mut StartFile)| {
        let mut f = file();
        change(&mut f);
        serde_json::to_string(&f).expect("json")
    };
    let nuls: [fn(&mut StartFile); 6] = [
        |f| f.asid_file.push('\0'),
        |f| f.profile.push('\0'),
        |f| f.program.push('\0'),
        |f| f.args.push("a\0".to_owned()),
        |f| f.params.push(("K\0".to_owned(), "v".to_owned())),
        |f| f.params.push(("K".to_owned(), "v\0".to_owned())),
    ];
    for change in nuls {
        assert_eq!(
            StartFile::parse(&with(change)),
            Err("the start file holds a NUL".to_owned())
        );
    }
    let mut value = serde_json::to_value(file()).expect("json");
    value["extra"] = serde_json::Value::Bool(true);
    let why = StartFile::parse(&value.to_string()).expect_err("unknown field");
    assert!(
        why.starts_with("the start file: unknown field `extra`"),
        "{why}"
    );
    assert!(StartFile::parse("not json").is_err());
}
