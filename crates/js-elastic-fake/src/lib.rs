//! An elastic fake cloud with a billing oracle of its own.
//!
//! [`ElasticFake`] implements [`js_elastic::CapacityProvider`] over memory, for tests of
//! the scaler and of a server that launches machines, and passes the contract's
//! conformance suite (`tests/conformance.rs`). It makes a test able to say what a
//! scheduler spends, from the cloud's side:
//!
//! - **Its own bill.** The cloud notes when it launched and ended each machine and bills
//!   them as the provider it stands for does ([`Billing`]): Lambda in one-minute
//!   increments, AWS per second with a 60-second minimum, Google Compute Engine and TPU
//!   per second, marked UNVERIFIED because their real terms were not read. The bill
//!   ([`ElasticFake::bill`], [`ElasticFake::bill_between`]) never consults the scaler's
//!   arithmetic, so a test that compares the two can catch an accounting bug; a test that
//!   compares the scaler with itself cannot.
//! - **What clouds do.** [`Step`]s script each call: a refusal for capacity, quota,
//!   denial or a request that does not fit; a call whose answer is lost and which may or
//!   may not have landed (the caller sees `Unknown`); a 429 storm; a list that fails
//!   forever; a release that is refused forever; a read by id with no answer.
//!   [`ElasticFake::script_next`] scripts the next call whatever its kind. [`Knobs`] set a
//!   quota, the room the cloud has, how long a machine boots, how long a release takes to
//!   show, and a lag in the list. [`ElasticFake::preempt`] reclaims a machine and sends a
//!   notice on every event stream; [`ElasticFake::add_foreign`] puts a machine in the
//!   account that the caller did not launch; [`ElasticFake::drop_machine`] removes one
//!   outside the caller.
//! - **Counted calls.** [`ElasticFake::calls`] lists every call, so a test can prove that
//!   no `acquire` was made. [`ElasticFake::read_handle`] gives a [`js_elastic::ReadHandle`],
//!   which cannot make one.
//!
//! # What the oracle does not model
//!
//! - **Instance time only.** Storage, addresses, NAT and egress, which bill when no instance
//!   runs (see the standing costs of the design), are not billed. A bound from this bill is a
//!   bound on instance spend.
//! - **Billing stops at the release call** (or the preemption), not at the end of the
//!   `terminating` state. A provider that bills until the machine is gone would bill more.
//! - **Preemption** stops billing at the moment the cloud reclaims the machine, for every
//!   [`Billing`]. Real providers differ (some do not charge a part of the last increment, some
//!   keep billing a stopped machine's disks); none of that is modelled.
//! - **AWS 60-second minimum** is applied once per machine from its launch. There is no
//!   restart of an instance, so the minimum that a restart would begin again does not exist.
//!
//! Time is a [`FakeClock`] the test moves. Nothing here reads the system clock, opens a
//! socket or names a real cloud; `tests/no_cloud_reach.rs` fails if anything in the
//! workspace can.

mod bill;
mod clock;
mod cloud;
mod script;

pub use bill::{Bill, Billing};
pub use clock::FakeClock;
pub use cloud::{ElasticFake, Knobs, PAYLOAD_LIMIT, PROVIDER_ID};
pub use script::{Call, Step, THROTTLE_WAIT};
