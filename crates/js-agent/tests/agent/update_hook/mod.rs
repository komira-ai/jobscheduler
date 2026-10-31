//! `js-agent update-hook` (host-updates design, 4.2), the verb skeleton: what the command
//! line, the last line and the params file accept and refuse, how an outcome becomes an exit
//! status, and that the verb is answered before the agent installs a signal handler. The steps
//! have no body yet; each is a stub that exits 1. The hook record, its lock, the declaration and
//! the rules the record is held to are pure of the steps and tested here on their own.

mod args;
mod declaration;
mod params;
mod record;
mod report;
mod validate;
mod verb;
