//! The first-launch runner end to end over the in-memory account, with a clock that
//! moves only when the runner sleeps. Nothing here reaches a network.

mod args;
mod go;
mod limits;
mod rig;
mod stop;
mod teardown;
