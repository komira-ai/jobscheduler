//! A server with a launching pool and the file of hosts the registry refused to remove
//! (`<state file>.scaler.unremoved`): the first tick retries them, and a file that cannot
//! be read stops the start. Time is the test's clock and the ticks are stepped; nothing
//! sleeps and no network is touched.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

use js_lease_server::UnremovedFile;

use crate::scaler_support::{
    UNREMOVED_KEY, account, kept_unremoved, put_in_store, start, start_refused, text, text_in,
    unremoved_path,
};
use crate::support::{Kept, Scratch};

/// The text that lists the one host to remove, as the file and the key hold it.
const LISTED: &str = r#"{"version":1,"hosts":["js-ghost"]}"#;

/// The text that lists none.
const NONE: &str = r#"{"version":1,"hosts":[]}"#;

/// Asserts that a server started on a file of hosts to remove tries them at its first
/// tick, and drops them from the file once the registry has taken them: the file lists a
/// host, the registry does not hold it (so it takes the removal at once), and after the
/// first tick the file lists none and is still owner-only. It runs with the hosts in their
/// file and in the state store (`acquires/scaler/unremoved`, which then lists none, with no
/// file beside the lease file). It catches a file or key that is read at start and then not
/// used, one whose entries are never written back out, a file written readable by others,
/// and a store mode that writes the file.
#[tokio::test]
async fn a_start_retries_the_hosts_in_the_file_and_drops_those_removed() {
    for kept in Kept::BOTH {
        retries_in(kept).await;
    }
}

async fn retries_in(kept: Kept) {
    let scratch = Scratch::new(&kept.named("scaler_unremoved_start"));
    account(&scratch);
    let path = unremoved_path(&scratch);
    let listed = BTreeSet::from(["js-ghost".to_owned()]);
    let source = text_in(&scratch, "live", "initialise", kept);
    match kept {
        Kept::Files => UnremovedFile::new(path.clone()).save(&listed).unwrap(),
        Kept::Store => put_in_store(&scratch, UNREMOVED_KEY, LISTED),
    }
    let h = start(scratch, &source).await;
    assert_eq!(kept_unremoved(&h).as_deref(), Some(LISTED));

    h.ticks.step().await;
    assert_eq!(kept_unremoved(&h).as_deref(), Some(NONE));
    match kept {
        Kept::Files => assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        ),
        Kept::Store => assert!(!path.exists(), "a store mode wrote the file"),
    }
}

/// Asserts that a server is refused at start, with a message that names the file, when the
/// file of hosts to remove is damaged, and that the same server starts when the file is
/// missing. It catches a damaged file read as an empty list, which would drop the retries
/// without a word.
#[tokio::test]
async fn a_damaged_file_stops_the_start_and_a_missing_one_does_not() {
    let scratch = Scratch::new("scaler_unremoved_damaged");
    account(&scratch);
    let source = text(&scratch, "live", "initialise");
    let path = unremoved_path(&scratch);
    let name = path.file_name().unwrap().to_str().unwrap().to_owned();
    scratch.write(&name, "{\"version\":1,\"hosts\":", 0o600);
    let said = start_refused(&scratch, &source).await;
    assert!(said.contains(&path.display().to_string()), "{said}");
    assert!(said.contains("unremoved-hosts file"), "{said}");

    std::fs::remove_file(&path).unwrap();
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    assert!(!path.exists());
}
