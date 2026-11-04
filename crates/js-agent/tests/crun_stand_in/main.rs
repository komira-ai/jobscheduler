//! `CrunRuntime` against a stand-in `crun`: a shell script that runs nothing but plays the
//! parts a container plays (exits, holds until SIGTERM, ignores it, is killed through the
//! lease's `cgroup.kill`, reports an error), and a node made of directories (no root, no
//! cgroup file system, no user namespace). What it shows is what the runtime asks of
//! `crun`, the files it makes and gives away, and how it reads a container's end; whether
//! a real `crun` honours the bundle is the `crun_runtime` target's.
//!
//! A container is run by a supervisor process (the agent's own binary, in its helper mode),
//! so it outlives the runtime that started it: a test that restarts the agent drops a runtime
//! and opens another, which adopts or removes what the first left (`adopt`, `sweep`).
//!
//! Nothing waits for a clock. A script that must be up before the test signals it opens a
//! named pipe the test reads (an `open` of a pipe blocks until the other end is opened);
//! the grace and the waits after a kill are given as zero when a test needs them spent.
#![cfg(target_os = "linux")]

mod adopt;
mod blind;
mod egress;
mod env;
mod gpus;
mod images;
mod node;
mod open;
mod pins;
mod runs;
mod stop;
mod sweep;
