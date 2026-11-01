//! Behaviour of the harness: the plan's order, the arithmetic of the numbers, the driver
//! against a simulated farm and a fake clock, the live farm against canned answers, and
//! the command line. None of it depends on timing.

mod binary;
mod cli;
mod clock;
mod driver;
mod exec;
mod fake;
mod http;
mod measure;
mod plan;
mod record;
mod reported;
mod stub;
mod summary;
mod support;
mod workload;
