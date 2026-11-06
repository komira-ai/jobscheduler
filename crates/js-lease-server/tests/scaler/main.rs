//! The scaler's shell and launch step through their public interface, over fakes: what the
//! shell writes first and does when a call fails, the order of a machine's whole life, the
//! marks it sets before a restarted server admits a hello, one launch rendered and handed
//! to a provider, the records file and the file of hosts the registry would not remove. Time
//! is an injected clock; nothing sleeps and no test touches a network. The demand and the
//! busy hosts the shell reads are the job tables': queued jobs and live allocations.

mod brake;
mod complete;
mod disk;
mod duties;
mod failover;
mod fakes;
mod jobs;
mod launcher;
mod lifetime;
mod notices;
mod records_file;
mod restart_hold;
mod restart_registry;
mod shell;
mod unremoved_file;
