//! `place`: one input tree placed by a mount provider on the Mac (tierstore's, on its NFS
//! loopback mount) through the agent's own provider client, for the tree routes of the
//! prototype: the directory it answers is what is shared into the VM or bound into a
//! container. `--release` gives the tree up again.

use std::io::Write;
use std::path::Path;
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
/// is still making the tree, or is not reachable yet) is asked again until [`GUARD`] has passed.
fn settle(client: &mut ProviderClient, call: &ProviderCall) -> ProviderAnswer {
    let began = super::clock::now();
    loop {
        match client.ask(NAME, call) {
            ProviderAnswer::Pending => {
                client.wait_answered(NAME, call, GUARD);
            }
            ProviderAnswer::Unavailable(why) => {
                if u128::from(super::clock::micros_since(began)) > GUARD.as_micros() {
                    return ProviderAnswer::Unavailable(why);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            answer => return answer,
        }
    }
}

/// Places (or releases) the tree and prints the directory (or `released`).
///
/// # Errors
/// The client could not be made, or the provider refused or failed.
pub fn run(place: &Place, out: &mut dyn Write) -> Result<(), String> {
    let mut client = ProviderClient::new(
        "linux-vm-poc",
        &[(
            NAME.to_owned(),
            Path::new(&place.socket).to_owned(),
            place.uid,
        )],
    )
    .map_err(|why| format!("the provider client: {why}"))?;
    let calls = if place.release {
        vec![ProviderCall::Release {
            run_id: place.run.clone(),
            index: 0,
        }]
    } else {
        vec![
            ProviderCall::Prepare {
                run_id: place.run.clone(),
                index: 0,
                reference: place.reference.clone(),
                mode: MountMode::ReadOnly,
            },
            ProviderCall::Mount {
                run_id: place.run.clone(),
                index: 0,
            },
        ]
    };
    for call in &calls {
        match settle(&mut client, call) {
            ProviderAnswer::Done(Some(dir)) => {
                let _ = writeln!(out, "{dir}");
            }
            ProviderAnswer::Done(None) => {}
            ProviderAnswer::Refused(why) => return Err(format!("refused: {why}")),
            ProviderAnswer::Unavailable(why) => return Err(format!("unavailable: {why}")),
            ProviderAnswer::Pending => return Err("no answer".to_owned()),
        }
    }
    if place.release {
        let _ = writeln!(out, "released");
    }
    Ok(())
}
