//! The lints of the workflows that publish js-agent to the apt repository and set its desired
//! version: the apt jobs of `release.yml` ([`apt`]), its install check ([`check_apt`]) and
//! `hostkeeper-desired.yml` ([`desired`]); and of the control-plane deploy of `release.yml`
//! ([`cp_deploy`]), each read from the YAML the workflow parses to
//! ([`yaml`], `saphyr-parser` events) and each shown to name every defect introduced into a workflow
//! that is otherwise clean (the `*_tests` modules, one table of defects each).
//!
//! The workflow `farm.yml` ([`farm`]), the advisory build on the build farm, is held the same way
//! (its `farm_tests` module), and so is every other workflow's grant of `id-token: write` on a pull
//! request, which only that job has.
//!
//! The job `pkg-upload` of `release.yml` ([`pkg_upload`]), which publishes the signed macOS
//! package of js-agent where `hostkeeper-desired.yml` finds it, is held the same way.
//!
//! The rules that read the files as text (pinned actions, permissions per job, only artifacts.yml
//! signs, the gamma step) are `tests/release_workflow`.

mod apt;
mod apt_tests;
mod check_apt;
mod check_apt_tests;
mod cp_deploy;
mod cp_deploy_tests;
mod desired;
mod desired_tests;
mod farm;
mod farm_steps;
mod farm_steps_tests;
mod farm_tests;
mod pkg_upload;
mod pkg_upload_tests;
mod support;
mod yaml;
mod yaml_tests;
