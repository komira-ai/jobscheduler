//! The lints of the workflows that publish js-agent to the apt repository and set its desired
//! version: the apt jobs of `release.yml` ([`apt`]), its install check ([`check_apt`]) and
//! `hostkeeper-desired.yml` ([`desired`]), each read from the YAML the workflow parses to
//! ([`yaml`], `saphyr-parser` events) and each shown to name every defect introduced into a workflow
//! that is otherwise clean (the `*_tests` modules, one table of defects each).
//!
//! The rules that read the files as text (pinned actions, permissions per job, only artifacts.yml
//! signs, the gamma step) are `tests/release_workflow`.

mod apt;
mod apt_tests;
mod check_apt;
mod check_apt_tests;
mod desired;
mod desired_tests;
mod support;
mod yaml;
mod yaml_tests;
