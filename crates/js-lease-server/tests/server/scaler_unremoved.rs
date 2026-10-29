//! A server with a launching pool and the file of hosts the registry refused to remove
//! (`<state file>.scaler.unremoved`): the first tick retries them, and a file that cannot
//! be read stops the start. Time is the test's clock and the ticks are stepped; nothing
//! sleeps and no network is touched.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

use js_lease_server::UnremovedFile;

use crate::scaler_support::{account, start, start_refused, text, unremoved_path};
use crate::support::Scratch;

/// Asserts that a server started on a file of hosts to remove tries them at its first
/// tick, and drops them from the file once the registry has taken them: the file lists a
/// host, the registry does not hold it (so it takes the removal at once), and after the
/// first tick the file lists none and is still owner-only. It catches a file that is read
/// at start and then not used, one whose entries are never written back out, and a file
/// written readable by others.
#[tokio::test]
async fn a_start_retries_the_hosts_in_the_file_and_drops_those_removed() {
    let scratch = Scratch::new("scaler_unremoved_start");
    account(&scratch);
    let path = unremoved_path(&scratch);
    let file = UnremovedFile::new(path.clone());
    file.save(&BTreeSet::from(["js-ghost".to_owned()])).unwrap();
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    assert_eq!(file.load().unwrap().len(), 1);

    h.ticks.step().await;
    assert_eq!(file.load().unwrap(), BTreeSet::new());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
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
