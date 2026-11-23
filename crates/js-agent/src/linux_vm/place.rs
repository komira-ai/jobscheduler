//! `place`: one input tree placed by a mount provider on the Mac (tierstore's, on its NFS
//! loopback mount) through the agent's own provider client, for the tree routes of the
//! prototype: the directory it answers is what is shared into the VM or bound into a
//! container. `--release` gives the tree up again.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::MountMode;

use crate::lifecycle::{ProviderAnswer, ProviderCall, Providers as _};
use crate::provider::ProviderClient;

/// The provider's name in the client (the client needs one).
const NAME: &str = "tierstore";

/// The longest wait for one answer: a far bound only against a provider that never answers.
const GUARD: Duration = Duration::from_secs(600);

/// What `place` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The provider's socket.
    pub socket: String,
    /// The uid the provider runs as (the owner its socket must have).
    pub uid: u32,
    /// The tree's reference.
    pub reference: String,
    /// The run the tree is placed for.
    pub run: String,
    /// Give the tree up instead of placing it.
    pub release: bool,
}

/// Asks `call` until the provider answers it for good. An `Unavailable` answer (the provider
/// is still making the tree, or is not reachable yet) is asked again, and a call still in
/// flight waited for, until `guard` has passed; then the last answer is returned.
fn settle(client: &mut ProviderClient, call: &ProviderCall, guard: Duration) -> ProviderAnswer {
    let began = super::clock::now();
    loop {
        let answer = client.ask(NAME, call);
        let waited = u128::from(super::clock::micros_since(began)) > guard.as_micros();
        let final_answer = !matches!(
            answer,
            ProviderAnswer::Pending | ProviderAnswer::Unavailable(_)
        );
        if waited || final_answer {
            return answer;
        }
        if answer == ProviderAnswer::Pending {
            let _ = client.wait_answered(NAME, call, guard);
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Places (or releases) the tree and prints the directory (or `released`).
///
/// # Errors
/// The client could not be made, or the provider refused or failed.
pub fn run(place: &Place, out: &mut dyn Write) -> Result<(), String> {
    run_within(place, GUARD, out, ProviderClient::new)
}

/// How the provider client is made: [`ProviderClient::new`], or a test's stand-in.
type MakeClient = fn(&str, &[(String, PathBuf, u32)]) -> std::io::Result<ProviderClient>;

/// [`run`], with `guard` the longest wait for one answer and `make` the client's maker.
fn run_within(
    place: &Place,
    guard: Duration,
    out: &mut dyn Write,
    make: MakeClient,
) -> Result<(), String> {
    let socket = (
        NAME.to_owned(),
        Path::new(&place.socket).to_owned(),
        place.uid,
    );
    let made = make("linux-vm-poc", &[socket]);
    let mut client = made.map_err(|why| format!("the provider client: {why}"))?;
    let (run_id, index) = (place.run.clone(), 0);
    let calls = if place.release {
        vec![ProviderCall::Release { run_id, index }]
    } else {
        let reference = place.reference.clone();
        let mode = MountMode::ReadOnly;
        let prepare = ProviderCall::Prepare {
            run_id: run_id.clone(),
            index,
            reference,
            mode,
        };
        vec![prepare, ProviderCall::Mount { run_id, index }]
    };
    for call in &calls {
        let why = match settle(&mut client, call, guard) {
            ProviderAnswer::Done(dir) => {
                let _ = dir.map(|dir| writeln!(out, "{dir}"));
                continue;
            }
            ProviderAnswer::Refused(why) => format!("refused: {why}"),
            other => format!("{other:?}"),
        };
        return Err(why);
    }
    if place.release {
        let _ = writeln!(out, "released");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_testkit::dir::TestDir;
    use js_testkit::mount_provider::{FakeProvider, mount_path};
    use std::os::unix::fs::PermissionsExt as _;

    fn rig(name: &str) -> (TestDir, FakeProvider, Place) {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let fake = FakeProvider::start(dir.path()).unwrap();
        std::fs::set_permissions(fake.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let place = Place {
            socket: fake.socket().to_str().unwrap().to_owned(),
            uid: rustix::process::geteuid().as_raw(),
            reference: "cas/l/ab".to_owned(),
            run: "r1".to_owned(),
            release: false,
        };
        (dir, fake, place)
    }

    fn placed(place: &Place, guard: Duration) -> (Result<(), String>, String) {
        let mut out = Vec::new();
        let result = run_within(place, guard, &mut out, ProviderClient::new);
        (result, String::from_utf8(out).unwrap())
    }

    /// Asserts that a tree is prepared then mounted and its directory printed, that a mount
    /// the provider cannot answer yet is asked again until it can, and that a release prints
    /// `released`. Catches a place that prints before the mount answers.
    #[test]
    fn a_tree_is_placed_retried_and_released() {
        let (_dir, fake, mut place) = rig("linux-vm-place");
        fake.fail_mount(2);
        let (result, out) = placed(&place, Duration::from_secs(60));
        assert_eq!(result, Ok(()));
        assert_eq!(out, format!("{}\n", mount_path("linux-vm-poc", "js.r1", 0)));
        place.release = true;
        assert_eq!(
            placed(&place, Duration::from_secs(60)),
            (Ok(()), "released\n".to_owned())
        );
    }

    /// Asserts that a ref the provider refuses is an error saying so, and that a provider that
    /// stays unavailable past the guard is an error naming the answer. Catches a place that
    /// exits 0 with no directory.
    #[test]
    fn a_refusal_and_a_provider_that_stays_down_are_errors() {
        let (_dir, fake, place) = rig("linux-vm-place-refused");
        fake.refuse("cas/l/ab");
        let (result, out) = placed(&place, Duration::from_secs(60));
        assert!(result.unwrap_err().starts_with("refused: "));
        assert_eq!(out, "");
        let (_dir2, fake2, place2) = rig("linux-vm-place-down");
        fake2.fail_mount(u32::MAX);
        // A guard of zero gives up at the first answer that is not final.
        let (result, _) = placed(&place2, Duration::ZERO);
        let why = result.unwrap_err();
        // The first answer to the first call is that it is in flight.
        assert_eq!(why, "Pending");
    }

    /// Asserts that `run` (the default guard) places a tree, as the verb calls it.
    #[test]
    fn run_places_with_the_default_guard() {
        let (_dir, _fake, place) = rig("linux-vm-place-run");
        let mut out = Vec::new();
        assert_eq!(run(&place, &mut out), Ok(()));
        assert!(!out.is_empty());
    }

    /// Asserts that a client that cannot be made (no threads or descriptors left) is an
    /// error naming it, and nothing is printed.
    #[test]
    fn a_client_that_cannot_be_made_is_an_error() {
        let (_dir, _fake, place) = rig("linux-vm-place-noclient");
        let mut out = Vec::new();
        let refuse: MakeClient = |_, _| Err(std::io::Error::other("no threads"));
        let result = run_within(&place, Duration::ZERO, &mut out, refuse);
        assert_eq!(result, Err("the provider client: no threads".to_owned()));
        assert!(out.is_empty());
    }
}
