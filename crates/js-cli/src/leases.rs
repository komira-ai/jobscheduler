//! The commands that talk to the server and print its answer.

use std::io::Write;
use std::time::Duration;

use crate::api::{Freed, Granted, Lease, Standing, entries};
use crate::cli::{AcquireArgs, IdArgs, RenewArgs};
use crate::client::Client;
use crate::events::{ForwardedSignal, Waiter};
use crate::failure::{Failure, exit};
use crate::render::Render;
use crate::tag::RunTag;

/// How often a pending start is looked at. The server settles pending starts on its
/// ticks (5s unless configured), so a tighter poll gains nothing.
const POLL: Duration = Duration::from_secs(2);

/// `lease acquire`: prints the lease once it is held.
pub(crate) fn acquire(
    client: &Client,
    render: Render,
    args: &AcquireArgs,
    waiter: &dyn Waiter,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let lease = obtain(client, args, waiter, &|| None, None)?;
    let _ = out.write_all(render.held(&lease).as_bytes());
    Ok(exit::OK)
}

/// Finds the leases that carry `tag` in the server's list and releases them: the way
/// back from a start whose answer was lost, when the start may have created a lease
/// nobody knows the id of. Returns the ids it released. One list and one release per
/// lease found, each as long as the client's limit at most; the first failure ends it.
pub(crate) fn recover(client: &Client, tag: &RunTag) -> Result<Vec<String>, Failure> {
    let listed = entries(&client.list()?)?;
    let marked: Vec<String> = listed
        .into_iter()
        .filter(|entry| tag.marks(entry))
        .map(|entry| entry.id)
        .collect();
    marked
        .iter()
        .try_for_each(|id| client.release(id).map(drop))?;
    Ok(marked)
}

/// Asks for a lease and, when the provider has not confirmed the start (202), looks at
/// the list every [`POLL`] until the lease is held or gone, for at most
/// `--wait-pending`. Time passes only through `waiter`: the wait is the sum of the
/// intervals asked of it. A lease still pending at the end is released (it would be
/// held later with nobody to renew it) and the failure is exit 5. So is a pending lease
/// whose poll fails, and one that `interrupt` reports a signal for before a poll (the
/// failure is then the signal's: 128 plus its number). A lease that is already held is
/// returned whatever `interrupt` says: the caller has the signal and acts on it. The
/// start is sent once, with `tag` (when given) among its labels.
pub(crate) fn obtain(
    client: &Client,
    args: &AcquireArgs,
    waiter: &dyn Waiter,
    interrupt: Interrupt,
    tag: Option<&RunTag>,
) -> Result<Lease, Failure> {
    match client.acquire(&args.body(tag))? {
        Granted::Held(lease) => Ok(lease),
        Granted::Pending(id) => settle(client, id, args.wait_pending, waiter, interrupt),
    }
}

/// Reports the signal `js` was sent, if one is waiting to be acted on.
pub(crate) type Interrupt<'a> = &'a dyn Fn() -> Option<ForwardedSignal>;

fn settle(
    client: &Client,
    id: String,
    limit: Duration,
    waiter: &dyn Waiter,
    interrupt: Interrupt,
) -> Result<Lease, Failure> {
    let mut waited = Duration::ZERO;
    loop {
        if let Some(signal) = interrupt() {
            let _ = client.release(&id);
            return Err(Failure::Interrupted(signal));
        }
        let standing = client.standing(&id).inspect_err(|_| {
            let _ = client.release(&id);
        })?;
        match standing {
            Standing::Held(lease) => return Ok(lease),
            Standing::Gone => return Err(Failure::Refused("pending-dropped".to_owned())),
            Standing::Pending => {}
        }
        if waited >= limit {
            let _ = client.release(&id);
            return Err(Failure::Unresolved {
                id,
                waited: waited.as_secs(),
            });
        }
        waiter.wait(POLL);
        waited += POLL;
    }
}

/// `lease renew`: prints the new expiry.
pub(crate) fn renew(
    client: &Client,
    render: Render,
    args: &RenewArgs,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let expires_at_ms = client.renew(&args.id, args.ttl)?;
    let _ = out.write_all(render.renewed(&args.id, expires_at_ms).as_bytes());
    Ok(exit::OK)
}

/// `lease release`: exit 0 when freed, 6 when the stop is not confirmed yet.
pub(crate) fn release(
    client: &Client,
    render: Render,
    args: &IdArgs,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let (state, code) = match client.release(&args.id)? {
        Freed::Freed => ("freed", exit::OK),
        Freed::Releasing => ("releasing", exit::RELEASING),
    };
    let _ = out.write_all(render.released(&args.id, state).as_bytes());
    Ok(code)
}

/// `lease list`: the server's leases.
pub(crate) fn list(client: &Client, render: Render, out: &mut dyn Write) -> Result<u8, Failure> {
    let body = client.list()?;
    let listing = render.listing(&body, &entries(&body)?);
    let _ = out.write_all(listing.as_bytes());
    Ok(exit::OK)
}
