//! An elastic fake cloud with a billing oracle of its own.
//!
//! [`ElasticFake`] implements [`js_elastic::ElasticProvider`] over memory, for tests of
//! the scaler and of a server that launches machines. It makes a test able to say what a
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
//!   forever; a release that is refused forever. [`Knobs`] set a quota, the room the cloud
//!   has, how long a machine boots, how long a release takes to show, and a lag in the
//!   list. [`ElasticFake::preempt`] reclaims a machine.
//! - **Counted calls.** [`ElasticFake::calls`] lists every call, so a test can prove that
//!   no `acquire` was made. [`ElasticFake::read_handle`] gives a [`js_elastic::ReadHandle`],
//!   which cannot make one.
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
pub use cloud::{ElasticFake, Knobs, PAYLOAD_LIMIT};
pub use script::{Call, Step, THROTTLE_WAIT};
