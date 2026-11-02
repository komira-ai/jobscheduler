//! What is refused before anything is read or done.

use std::ffi::OsString;
use std::io::{self, Read};

use super::rig::{INSTANCE, PROVIDER, Rig, TOKEN, status};

/// A standard input that fails the test when it is read.
struct Untouched;

impl Read for Untouched {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("standard input was read");
    }
}

/// Asserts that a user who is not root is refused (status 2, "must run as root") in a dry run
/// and in an apply, that standard input is not read, the setup is not run and nothing is
/// written. It catches a root check that comes after the first change.
#[test]
fn a_user_who_is_not_root_is_refused_and_nothing_is_touched() {
    for flags in [&["--apply"][..], &[][..]] {
        let mut rig = Rig::new("enroll_not_root");
        rig.host.euid = 1000;
        let mut args: Vec<OsString> = vec!["js-agent".into(), "enroll".into()];
        args.extend(["--provider", PROVIDER, "--instance", INSTANCE].map(OsString::from));
        args.extend(flags.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = js_agent::enroll::run(&args, &mut Untouched, &mut out, &mut err, &rig.host);
        assert_eq!(format!("{code:?}"), status(2));
        assert_eq!(out, b"");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("must run as root"), "{err}");
        assert_eq!(rig.setup_args(), None);
        assert_eq!(rig.names(), Vec::<String>::new());
    }
}

/// Asserts that a provider or an instance that the server would refuse (and that no hello
/// could carry) is refused here (status 2) naming the flag, whatever else is right. It
/// catches an instance file that the agent would read and the server would not admit.
#[test]
fn a_provider_or_instance_the_server_would_refuse_is_refused() {
    let long = "a".repeat(129);
    let bad: [(&str, &str, &str); 6] = [
        ("a:b", INSTANCE, "provider"),
        ("", INSTANCE, "provider"),
        (&long, INSTANCE, "provider"),
        (PROVIDER, "i 0", "instance"),
        (PROVIDER, "", "instance"),
        (PROVIDER, "_lead", "instance"),
    ];
    for (provider, instance, field) in bad {
        let rig = Rig::new("enroll_bad_ids");
        let ran = rig.run(
            &["--provider", provider, "--instance", instance, "--apply"],
            TOKEN,
        );
        assert_eq!(ran.status, status(2), "{provider:?} {instance:?}: {ran:?}");
        assert!(
            ran.err.contains(&format!("instance.{field}")) || ran.err.contains("instance_id"),
            "{provider:?} {instance:?}: {ran:?}"
        );
        assert_eq!(rig.setup_args(), None);
        assert_eq!(rig.names(), Vec::<String>::new());
    }
}

/// Asserts the edges of the two ids, which follow different rules: a provider is a provider
/// id (lower case, 1 to 63 bytes), so 63 bytes is taken and 64 bytes or an upper-case letter
/// is refused naming `instance.provider`; an instance id takes upper case and up to 128
/// bytes, so 128 bytes and `I-0ABC` are taken and 129 bytes is refused. A dry run is enough:
/// the ids are checked before the setup. It catches one rule documented or applied for both.
#[test]
fn the_provider_and_the_instance_have_their_own_rules() {
    let (p63, p64) = ("p".repeat(63), "p".repeat(64));
    let (i128, i129) = ("i".repeat(128), "i".repeat(129));
    let taken: [(&str, &str); 3] = [(&p63, INSTANCE), (PROVIDER, &i128), (PROVIDER, "I-0ABC")];
    for (provider, instance) in taken {
        let rig = Rig::new("enroll_ids_taken");
        let ran = rig.run(&["--provider", provider, "--instance", instance], TOKEN);
        assert_eq!(ran.status, status(0), "{provider:?} {instance:?}: {ran:?}");
        assert!(
            ran.out.ends_with(&format!(
                "status=planned provider={provider} instance={instance}\n"
            )),
            "{ran:?}"
        );
    }
    let refused: [(&str, &str, &str); 3] = [
        (&p64, INSTANCE, "instance.provider"),
        ("Burst", INSTANCE, "instance.provider"),
        (PROVIDER, &i129, "instance.instance_id"),
    ];
    for (provider, instance, field) in refused {
        let rig = Rig::new("enroll_ids_refused");
        let ran = rig.run(&["--provider", provider, "--instance", instance], TOKEN);
        assert_eq!(ran.status, status(2), "{provider:?} {instance:?}: {ran:?}");
        assert!(ran.err.contains(field), "{field}: {ran:?}");
        assert_eq!(rig.setup_args(), None);
    }
}
