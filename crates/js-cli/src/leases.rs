//! The commands that talk to the server and print its answer.

use std::io::Write;
use std::time::Duration;

use crate::api::{Freed, Granted, Lease, Standing, entries};
use crate::cli::{AcquireArgs, IdArgs, RenewArgs};
use crate::client::Client;
use crate::events::Waiter;
use crate::failure::{Failure, exit};
use crate::render::Render;

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
    let lease = obtain(client, args, waiter)?;
    let _ = out.write_all(render.held(&lease).as_bytes());
    Ok(exit::OK)
}

/// Asks for a lease and, when the provider has not confirmed the start (202), looks at
/// the list every [`POLL`] until the lease is held or gone, for at most
/// `--wait-pending`. Time passes only through `waiter`: the wait is the sum of the
/// intervals asked of it. A lease still pending at the end is released (it would be
/// held later with nobody to renew it) and the failure is exit 5.
pub(crate) fn obtain(
    client: &Client,
    args: &AcquireArgs,
    waiter: &dyn Waiter,
) -> Result<Lease, Failure> {
    match client.acquire(&args.body())? {
        Granted::Held(lease) => Ok(lease),
        Granted::Pending(id) => settle(client, id, args.wait_pending, waiter),
    }
}

fn settle(
    client: &Client,
    id: String,
    limit: Duration,
    waiter: &dyn Waiter,
) -> Result<Lease, Failure> {
    let mut waited = Duration::ZERO;
    loop {
        match client.standing(&id)? {
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
