use super::testing::*;
use super::*;

#[test]
fn a_failed_aws_call_is_told_by_what_it_says() {
    // Catches: a lost race read as a failure (or the reverse), and a missing key read as a denial.
    assert_eq!(classify(PRECONDITION), Failure::Precondition);
    assert_eq!(
        classify("An error occurred (ConditionalRequestConflict) when calling"),
        Failure::Precondition
    );
    assert_eq!(
        classify("a response (412) came back"),
        Failure::Precondition
    );
    assert_eq!(classify(NO_SUCH_KEY), Failure::NotFound);
    assert_eq!(classify("Not Found"), Failure::NotFound);
    assert_eq!(classify(DENIED), Failure::Denied);
    assert_eq!(classify("Forbidden"), Failure::Denied);
    assert_eq!(classify("could not connect"), Failure::Other);
}

#[test]
fn the_package_files_of_a_version_are_one_per_architecture_and_must_all_be_there() {
    // Catches: an index or upload for one architecture only, or for another version's files.
    let dir = scratch("package-files");
    release_files(&dir, "1.2.3");
    let files = package_files(&dir, "1.2.3").expect("files");
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["js-agent_1.2.3_amd64.deb", "js-agent_1.2.3_arm64.deb"]
    );
    assert_eq!(files[0].1, dir.join("js-agent_1.2.3_amd64.deb"));
    std::fs::remove_file(dir.join("js-agent_1.2.3_arm64.deb")).expect("remove");
    let e = package_files(&dir, "1.2.3").unwrap_err().to_string();
    assert!(e.contains("js-agent_1.2.3_arm64.deb is not in"), "{e}");
    assert!(package_files(&dir, "1.2.4").is_err());
    assert!(matches!(
        package_files(&dir, "v1.2.3"),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_write_verb_with_a_pool_half_given_is_a_usage_error() {
    // Catches: a pool of one half (debs without a version) read as a pool or as none; clap holds
    // the flags together on the command line, and the verb holds them again.
    let verb = |debs: Option<&str>, version: Option<&str>, no_pool: bool| Verb::Write {
        suite: Suite::Stable,
        bucket: "apt-bucket".to_owned(),
        distribution_id: "E1".to_owned(),
        hostname: "packages.komira.ai".to_owned(),
        next: PathBuf::from("next"),
        debs: debs.map(PathBuf::from),
        version: version.map(str::to_owned),
        no_pool,
        last: true,
        output: None,
        aws: PathBuf::from("aws"),
    };
    for v in [
        verb(Some("d"), None, false),
        verb(None, None, false),
        verb(Some("d"), Some("1.2.3"), true),
    ] {
        let e = run(v).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
    }
}

#[test]
fn each_apt_verb_is_dispatched_with_its_own_flags() {
    // Catches: a verb wired to another verb's function, or a flag dropped on the way: every
    // verb runs against directories and programs that are not there, and fails naming the thing
    // it was given.
    let dir = scratch("dispatch");
    let nowhere = |name: &str| dir.join(name);
    let said = |result: Result<Vec<String>, Error>| result.unwrap_err().to_string();

    let e = said(run(Verb::Read {
        suite: Suite::Testing,
        bucket: BUCKET.to_owned(),
        out: nowhere("read"),
        aws: nowhere("no-aws"),
    }));
    assert!(e.contains("no-aws"), "{e}");

    let e = said(run(Verb::Index {
        suite: Suite::Testing,
        current: nowhere("current"),
        out: nowhere("next"),
        debs: nowhere("debs"),
        version: "1.2.3".to_owned(),
        origin: "o".to_owned(),
        label: "l".to_owned(),
        description: "d".to_owned(),
        date: 1_790_000_000,
        keep: 25,
    }));
    assert!(e.contains("current/etags.txt is missing"), "{e}");

    let e = said(run(Verb::Promote {
        testing: nowhere("testing"),
        stable: nowhere("stable"),
        out: nowhere("promoted"),
        version: "1.2.3".to_owned(),
        sums: nowhere("SHA256SUMS"),
        origin: "o".to_owned(),
        label: "l".to_owned(),
        description: "d".to_owned(),
        date: 1_790_000_000,
        keep: 25,
    }));
    assert!(
        e.contains("stable/etags.txt is missing: run `apt read --suite stable` first"),
        "{e}"
    );

    let key = nowhere("key.asc");
    let passphrase = nowhere("passphrase");
    std::fs::write(&key, "key").expect("key");
    std::fs::write(&passphrase, "phrase").expect("passphrase");
    let e = said(run(Verb::Sign {
        dir: nowhere("dists"),
        work: nowhere("work"),
        fingerprint: "0123456789ABCDEF0123456789ABCDEF01234567".to_owned(),
        key_file: Some(key.clone()),
        key_env: None,
        passphrase_file: Some(passphrase.clone()),
        passphrase_env: None,
        gpg: nowhere("no-gpg"),
        output: None,
    }));
    assert!(
        e.contains("dists/Release does not exist: nothing to sign"),
        "{e}"
    );
    assert!(!key.exists() && !passphrase.exists());

    let e = said(run(Verb::Write {
        suite: Suite::Stable,
        bucket: BUCKET.to_owned(),
        distribution_id: "E1".to_owned(),
        hostname: "packages.komira.ai".to_owned(),
        next: nowhere("next"),
        debs: None,
        version: None,
        no_pool: true,
        last: false,
        output: None,
        aws: nowhere("no-aws"),
    }));
    assert!(e.contains("next"), "{e}");

    let e = said(run(Verb::Write {
        suite: Suite::Testing,
        bucket: BUCKET.to_owned(),
        distribution_id: "E1".to_owned(),
        hostname: "packages.komira.ai".to_owned(),
        next: nowhere("next"),
        debs: Some(nowhere("debs")),
        version: Some("1.2.3".to_owned()),
        no_pool: false,
        last: true,
        output: None,
        aws: nowhere("no-aws"),
    }));
    assert!(e.contains("debs"), "{e}");
}
