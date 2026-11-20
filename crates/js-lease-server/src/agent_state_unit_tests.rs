//! Unit tests of the host bindings' file and store ([`super`]).

use js_testkit::dir::TestDir;

use super::*;

fn bound(token: &str, instance: Option<(&str, &str)>) -> Binding {
    let instance = instance.map(|(provider, instance_id)| InstanceIdentity {
        provider: provider.to_string(),
        instance_id: instance_id.to_string(),
    });
    Binding::new(Credential::of_token(token), instance)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

const CREDENTIAL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A taint commit over a file writes nothing and succeeds: the file is saved after the
/// registry takes the change, and no other server shares it. It catches a commit that
/// reaches for a store the server does not have.
#[test]
fn a_taint_commit_over_a_file_writes_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "agent_state_unit-taint-file");
    let path = dir.join("taint.agents");
    let store = BindingStore::new(path.clone(), BTreeMap::new());
    let change = store.change(&host("a"), bound("token-a", None));
    assert!(change.writes.is_empty());
    store.commit_taint(&change, None).unwrap();
    assert!(!path.exists());
}

/// The snapshot is taken while the store's lock is held, so of two saves the one that
/// writes last holds the newest bindings. It catches a save that reads the registry
/// first and locks after, which lets a slower save put an older snapshot over a newer
/// file; the closure runs in the place of that read and finds the lock taken or not.
#[test]
fn the_snapshot_is_taken_under_the_lock() {
    let dir = TestDir::under(std::env::temp_dir(), "agent_state_unit-snapshot");
    let path = dir.join("snapshot.agents");
    let store = BindingStore::new(path.clone(), BTreeMap::new());
    let mut bindings = BTreeMap::new();
    bindings.insert(host("a"), bound("token-a", None));
    let mut locked = false;
    store
        .save(|| {
            locked = store.written.try_lock().is_err();
            bindings.clone()
        })
        .unwrap();
    assert!(locked, "the snapshot was read before the store's lock");
    assert_eq!(load(&path).unwrap(), bindings);
}

/// What is written reads back as the same bindings, with and without an instance, and
/// the text holds a digest and no token. It catches a binding that comes back as
/// another credential or without its instance (a restart that lets a different
/// machine take the lease) and a file that carries the secret itself.
#[test]
fn bindings_survive_the_file() {
    let mut bindings = BTreeMap::new();
    bindings.insert(host("a"), bound("token-a", None));
    bindings.insert(host("b"), bound("token-b", Some(("aws", "i-0abc"))));
    let text = render(&bindings).unwrap();
    assert_eq!(parse(&text).unwrap(), bindings);
    assert!(text.contains("version = 1"), "{text}");
    assert!(!text.contains("token-a"), "{text}");
    assert!(!text.contains("token-b"), "{text}");
    assert_eq!(parse("version = 1\n").unwrap(), BTreeMap::new());
}

/// A taint, with and without its run and check, reads back as written, and a host
/// without one reads back without. It catches a restart that makes a tainted host
/// clean, and one that taints a host that was not.
#[test]
fn a_taint_survives_the_file() {
    let taint = |run_id: Option<&str>, check: Option<&str>| Taint {
        reason: "residue".to_string(),
        run_id: run_id.map(str::to_string),
        check: check.map(str::to_string),
    };
    let mut bindings = BTreeMap::new();
    bindings.insert(
        host("a"),
        bound("t", None).with_taint(Some(taint(None, None))),
    );
    bindings.insert(
        host("b"),
        bound("t", None).with_taint(Some(taint(Some("run-1-1"), Some("tmp")))),
    );
    bindings.insert(host("c"), bound("t", None));
    let text = render(&bindings).unwrap();
    assert_eq!(parse(&text).unwrap(), bindings);
    assert_eq!(text.matches("taint_reason").count(), 2, "{text}");
}

/// A run or a check of a taint with no reason is a file that cannot be read as
/// written. It catches a taint that loads as no taint.
#[test]
fn a_taint_without_a_reason_is_refused() {
    for extra in ["taint_run_id = \"r\"\n", "taint_check = \"c\"\n"] {
        let text =
            format!("version = 1\n[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n{extra}");
        let reason = parse(&text).unwrap_err();
        assert!(reason.contains("taint"), "{reason}");
    }
}

/// A later version's keys, at the top and in an entry, are ignored whatever the
/// version number says, and a file with no version at all is refused as not this
/// file: the format is additive, so an older build reads a newer file. It catches a
/// reader that refuses a file for a key it does not know, and one that takes any
/// TOML with a `host` table for its own.
#[test]
fn unknown_keys_are_ignored() {
    let text = format!(
        "version = 9\nfuture = true\n[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\nnote = \"x\"\n"
    );
    let parsed = parse(&text).unwrap();
    assert_eq!(parsed.len(), 1);
    let unversioned = format!("[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n");
    assert!(parse(&unversioned).is_err());
}

/// Every way a file can be wrong is refused with a reason that names the fault and
/// repeats nothing of the entry: text that is not TOML, a bad host id, a credential
/// that is not a digest, half an instance, and a host listed twice. It catches a
/// file that loads as a smaller set of bindings (a start that quietly drops the
/// protection it exists to give).
#[test]
fn a_file_that_cannot_be_read_as_written_is_refused() {
    let entry = |id: &str, credential: &str, extra: &str| {
        format!("version = 1\n[[host]]\nid = \"{id}\"\ncredential = \"{credential}\"\n{extra}")
    };
    let twice = format!(
        "{}[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n",
        entry("a", CREDENTIAL, "")
    );
    let cases = [
        ("version = [".to_string(), ""),
        (entry("Bad Id", CREDENTIAL, ""), "host id"),
        (entry("a", "abc", ""), "credential"),
        (entry("a", CREDENTIAL, "instance_id = \"i\"\n"), "instance"),
        (
            entry("a", CREDENTIAL, "instance_provider = \"aws\"\n"),
            "instance",
        ),
        (twice, "twice"),
    ];
    for (text, names) in cases {
        let reason = parse(&text).unwrap_err();
        assert!(reason.contains(names), "{names}: {reason}");
        assert!(!reason.contains("Bad Id"), "{reason}");
        assert!(!reason.contains(CREDENTIAL), "{reason}");
    }
    let both = entry(
        "a",
        CREDENTIAL,
        "instance_provider = \"aws\"\ninstance_id = \"i\"\n",
    );
    assert!(parse(&both).is_ok());
}

/// A temporary file left beside the target by a crash, with a wider mode and other
/// bytes, does not decide the mode of what is written: the result is owner-only and
/// holds the new bytes. A temporary name that cannot be removed (a directory) is an
/// error and the target is untouched. It catches a rename of a world-readable stale file
/// into place.
#[test]
fn a_stale_temporary_file_does_not_decide_the_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::under(std::env::temp_dir(), "agent_state_unit-stale");
    let path = dir.join("stale.state");
    let temp = dir.join("stale.state.tmp");
    fs::write(&temp, "left by a crash").unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o644)).unwrap();
    replace(&path, b"new").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!temp.exists());

    fs::create_dir(&temp).unwrap();
    assert!(replace(&path, b"newer").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    fs::remove_dir(&temp).unwrap();
}
