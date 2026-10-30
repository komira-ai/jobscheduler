//! The scaler's shell and launch step through their public interface, over fakes: what the
//! shell writes first and does when a call fails, the order of a machine's whole life, the
//! marks it sets before a restarted server admits a hello, one launch rendered and handed
//! to a provider, the records file and the file of hosts the registry would not remove. Time
//! is an injected clock; nothing sleeps and no test touches a network.

mod complete;
mod duties;
mod fakes;
mod launcher;
mod records_file;
mod restart_hold;
mod restart_registry;
mod shell;
mod unremoved_file;
