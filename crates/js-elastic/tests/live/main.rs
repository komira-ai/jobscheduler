//! The live conformance driver against an in-memory provider: the arguments it reads, the
//! plan and its hash, the dry run that launches nothing, the spend run that passes, and
//! every way a spend run goes wrong (a failing case, a leftover machine, a failing list,
//! a provider that cannot be built), each of which still ends in the cleanup.

mod failures;
mod mem;
mod parse;
mod plan;
mod rig;
