//! Behaviour of the lease table through its public interface, over the fake provider
//! and the static pool, with a clock the tests advance. One test binary, in modules.

mod acquire;
mod ceiling;
mod crash;
mod cross_release;
mod expiry;
mod ids;
mod persistence;
mod renew_release;
mod reservations;
mod shares;
mod slow_save;
mod support;
mod timings;
mod whole;
