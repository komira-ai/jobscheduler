//! The scaler's shell and launch step through their public interface, over fakes: what the
//! shell writes first and does when a call fails, the order of a machine's whole life, the
//! marks it sets before a restarted server admits a hello, one launch rendered and handed
//! to a provider, and the records file. Time is an injected clock; nothing sleeps and no
//! test touches a network.

mod fakes;
mod launcher;
mod records_file;
mod shell;
