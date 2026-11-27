//! J5: the released agent beside a mount provider that cannot always serve.
//!
//! The provider is the fake (`js-fake-mount-provider`), on the socket the farm's flags name for
//! tierstore's, set through its control socket (`--provider-control`) to answer as tierstore
//! does: a tree placed at Mount, [`MAX_PLACEMENTS`] placements at once, a store that stalls (a
//! Mount held, then "not ready"), a restart that forgets the starts it was preparing, and
//! answers of `UNAVAILABLE` to every call. Every call it answered, with the time it came, is
//! read back from the control socket, and the pass conditions are counted on those times.
//!
//! The scenarios, each from a host in a session with the released agent:
//!
//! - `j5-burst`: [`BURST`] starts while the store stalls for [`STALL`], after which it holds
//!   [`MAX_PLACEMENTS`] placements, refused at Mount as tierstore refuses today;
//! - `j5-burst-at-prepare`: [`BURST`] starts with the limit met at Prepare (the provider places
//!   at Prepare and refuses a new source there), as tierstore answered when the agent's retry
//!   storm was seen;
//! - `j5-restart`: [`WAITING`] starts wait on a stalled store, the provider restarts, the store
//!   comes back;
//! - `j5-outage`: [`WAITING`] starts while the provider answers every call unavailable for
//!   [`PROVIDER_OUTAGE`];
//! - `j5-refused`: one start whose ref the provider refuses.
//!
//! Each leaves the provider serving with no limit to speak of, as the rig's other scenarios
//! need it, whatever it came to; one that did not pass also cancels the jobs it left
//! ([`guarded`]).

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Duration;

use js_blackbox::{
    BURST, CALL_WINDOW, JobSeen, MAX_PLACEMENTS, MOST_CALLS_IN_WINDOW, PROVIDER_OUTAGE,
    PROVIDER_REFUSED, ProviderCall, STALL, Seen, WAITING, busiest, own_fault, per_second,
    provider_calls,
};
use serde_json::json;

use super::{JOB_LIMIT, Rig};
use crate::watch;

/// The placement limit the provider is left with: tierstore's default.
const SERVING_LIMIT: usize = 1024;

/// The ref of a tree the provider refuses (the fake refuses every ref with this beginning).
const REFUSED_REF: &str = "refused/blackbox-tree";

/// How long the fake provider has to answer a command on its control socket.
const CONTROL_LIMIT: Duration = Duration::from_secs(60);

/// The fake provider's control socket.
struct Provider<'a>(&'a Path);

impl Provider<'_> {
    /// The reply to `line`.
    fn tell(&self, line: &str) -> Result<String, String> {
        let mut stream = UnixStream::connect(self.0)
            .map_err(|e| format!("the provider's control socket {}: {e}", self.0.display()))?;
        stream
            .set_read_timeout(Some(CONTROL_LIMIT))
            .and_then(|()| stream.set_write_timeout(Some(CONTROL_LIMIT)))
            .map_err(|e| format!("the provider's control socket: {e}"))?;
        stream
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("tell the provider {line:?}: {e}"))?;
        let mut reply = String::new();
        stream
            .read_to_string(&mut reply)
            .map_err(|e| format!("the provider's reply to {line:?}: {e}"))?;
        Ok(reply)
    }

    /// Tells the provider `line`, which it must answer `ok`.
    fn set(&self, line: &str) -> Result<(), String> {
        match self.tell(line)?.as_str() {
            "ok\n" => Ok(()),
            other => Err(format!("the provider answered {line:?} with {other:?}")),
        }
    }

    /// Every call it answered since its last reset.
    fn calls(&self) -> Result<Vec<ProviderCall>, String> {
        provider_calls(&self.tell("calls")?)
    }

    /// Waits until `count` calls of `method` have been answered, and returns them all.
    fn wait_calls(&self, method: &str, count: usize) -> Result<Vec<ProviderCall>, String> {
        let mut seen = Vec::new();
        watch(|w| {
            w.until(JOB_LIMIT, &mut || match self.calls() {
                Ok(calls) if calls.iter().filter(|c| c.method == method).count() >= count => {
                    seen = calls;
                    Seen::Yes
                }
                other => Seen::No(format!("{other:?}")),
            })
        })
        .within(
            &format!("{count} {method} calls reach the provider"),
            JOB_LIMIT,
        )?;
        Ok(seen)
    }

    /// Leaves the provider as the rig's other scenarios need it: serving, no limit to speak of.
    fn leave(&self) -> Result<(), String> {
        self.set(&format!("reset {SERVING_LIMIT} mount"))
    }
}

impl Rig {
    /// The fake provider's control socket.
    fn provider(&self) -> Provider<'_> {
        Provider(&self.flags.provider_control)
    }

    /// Submits `count` jobs named `name` that sleep a second with the tree `reference` of the
    /// provider mounted, each booking a small share of the host, so that all of them start at
    /// once: their ids.
    fn submit_burst(&self, name: &str, count: usize, reference: &str) -> Vec<String> {
        (0..count)
            .map(|_| {
                let mounts = json!([{
                    "provider": {"provider": self.flags.provider, "ref": reference},
                    "target": "/work/in"
                }]);
                self.submit_sized(name, 1, mounts, ("50", "33554432"))
            })
            .collect()
    }

    /// Waits for every job of `ids` to end: how each ended.
    fn all_ended(&self, ids: &[String]) -> Result<Vec<(String, JobSeen)>, String> {
        let mut ended = Vec::new();
        for id in ids {
            let mut last = None;
            watch(|w| {
                w.until(JOB_LIMIT, &mut || match self.job(id) {
                    Ok(j) if j.ended() => {
                        last = Some(j);
                        Seen::Yes
                    }
                    other => Seen::No(format!("{other:?}")),
                })
            })
            .within(&format!("job {id} ends"), JOB_LIMIT)
            .map_err(|e| format!("{e}\n{}", self.journal()))?;
            ended.extend(last.map(|j| (id.clone(), j)));
        }
        Ok(ended)
    }
}

/// Runs the scenario `body`, which gathers the jobs it submits, and then, whatever it came to
/// (a panic included): leaves the provider serving, and when the scenario did not pass, cancels
/// the jobs it left and waits for them to end, so that none of them calls the provider in the
/// next scenario.
fn guarded(
    rig: &Rig,
    body: fn(&Rig, &mut Vec<String>) -> Result<(), String>,
) -> Result<(), String> {
    let mut jobs = Vec::new();
    let outcome = catch_unwind(AssertUnwindSafe(|| body(rig, &mut jobs)))
        .unwrap_or_else(|_| Err("the scenario panicked (its message is above)".to_owned()));
    let left = rig.provider().leave();
    if outcome.is_err() {
        for id in &jobs {
            let _ = rig
                .client()
                .post(&format!("/v1/jobs/{id}:cancel"), &json!({}));
        }
        let _ = rig.all_ended(&jobs);
    }
    outcome.and(left)
}

/// How the jobs ended, one line each, for a failure's report.
fn ends(ended: &[(String, JobSeen)]) -> String {
    let lines: Vec<String> = ended
        .iter()
        .map(|(id, j)| {
            format!(
                "{id}: {} {} {}",
                j.state, j.start_failure, j.start_failure_detail
            )
        })
        .collect();
    lines.join("\n")
}

/// The calls of Prepare and Mount per second and the busiest window of each, for a report.
fn counted(calls: &[ProviderCall]) -> String {
    ["Prepare", "Mount"]
        .map(|method| {
            let most = busiest(calls, method, CALL_WINDOW);
            format!(
                "{method}: {} in the busiest 30 s (from {:.1} s); per second {}",
                most.count,
                most.from.as_secs_f64(),
                per_second(calls, method)
            )
        })
        .join("\n")
}

/// What is wrong with a burst: more calls of a kind in a window than the limit, a job that
/// ended as its own fault, or no Release of a job.
fn judge_burst(calls: &[ProviderCall], ended: &[(String, JobSeen)]) -> Vec<String> {
    let mut wrong: Vec<String> = ["Prepare", "Mount"]
        .into_iter()
        .map(|method| (method, busiest(calls, method, CALL_WINDOW)))
        .filter(|(_, most)| most.count > MOST_CALLS_IN_WINDOW)
        .map(|(method, most)| {
            format!(
                "{} {method} calls reached the provider in 30 s (from {:.1} s), more than {MOST_CALLS_IN_WINDOW}",
                most.count,
                most.from.as_secs_f64()
            )
        })
        .collect();
    wrong.extend(ended.iter().filter(|(_, j)| own_fault(j)).map(|(id, j)| {
        format!(
            "{id} ended as its own fault: {} {} {}",
            j.state, j.start_failure, j.start_failure_detail
        )
    }));
    wrong
}

/// The verdict: `Ok` when nothing is wrong, otherwise each wrong thing, then the calls counted
/// and how the jobs ended.
fn verdict(
    wrong: &[String],
    calls: &[ProviderCall],
    ended: &[(String, JobSeen)],
) -> Result<(), String> {
    match wrong.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "{}\n{}\n{}",
            wrong.join("\n"),
            counted(calls),
            ends(ended)
        )),
    }
}

/// `j5-burst`: catches an agent that asks again at once when the provider cannot serve a start
/// (the retry storm of a provider at its placement limit, about 1,000 calls a second), a store
/// refusal that fails a start as the job's own fault, and a start left waiting when the
/// Releases free placements.
fn burst_body(rig: &Rig, jobs: &mut Vec<String>) -> Result<(), String> {
    rig.steady()?;
    let provider = rig.provider();
    provider.set(&format!("reset {MAX_PLACEMENTS} mount"))?;
    provider.set("stall")?;
    let ids = rig.submit_burst("j5-burst", BURST, "blackbox-tree");
    jobs.extend(ids.iter().cloned());
    provider.wait_calls("Prepare", BURST)?;
    watch(|w| w.hold(STALL, &mut || Ok(()))).kept("the store's stall")?;
    let held = provider.calls()?;
    provider.set("serve")?;
    let ended = rig.all_ended(&ids)?;
    let calls = provider.wait_calls("Release", BURST)?;
    let mut wrong = judge_burst(&calls, &ended);
    if !held
        .iter()
        .any(|c| c.method == "Mount" && c.answer == "unavailable")
    {
        wrong.push(
            "no Mount was answered unavailable while the store stalled: no start waited on it"
                .to_owned(),
        );
    }
    verdict(&wrong, &calls, &ended)
}

/// `j5-burst-at-prepare`: the same burst against a provider that places at Prepare and refuses a
/// new source there at its limit; catches the retry storm on Prepare.
fn burst_at_prepare_body(rig: &Rig, jobs: &mut Vec<String>) -> Result<(), String> {
    rig.steady()?;
    let provider = rig.provider();
    provider.set(&format!("reset {MAX_PLACEMENTS} prepare"))?;
    let ids = rig.submit_burst("j5-prepare", BURST, "blackbox-tree");
    jobs.extend(ids.iter().cloned());
    let ended = rig.all_ended(&ids)?;
    let calls = provider.wait_calls("Release", BURST)?;
    let mut wrong = judge_burst(&calls, &ended);
    if !calls
        .iter()
        .any(|c| c.method == "Prepare" && c.answer == "unavailable")
    {
        wrong.push("no Prepare was refused at the limit: no start waited on it".to_owned());
    }
    verdict(&wrong, &calls, &ended)
}

/// `j5-restart`: catches an agent that, when the provider restarts and forgets the starts it
/// was preparing, keeps asking it to Mount them instead of sending Prepare again.
fn restart_body(rig: &Rig, jobs: &mut Vec<String>) -> Result<(), String> {
    rig.steady()?;
    let provider = rig.provider();
    provider.set(&format!("reset {MAX_PLACEMENTS} mount"))?;
    provider.set("stall")?;
    let ids = rig.submit_burst("j5-restart", WAITING, "blackbox-tree");
    jobs.extend(ids.iter().cloned());
    provider.wait_calls("Prepare", WAITING)?;
    provider.set("restart")?;
    provider.wait_calls("Restart", 1)?;
    provider.set("serve")?;
    let ended = rig.all_ended(&ids)?;
    let calls = provider.wait_calls("Release", WAITING)?;
    let restarted = calls
        .iter()
        .position(|c| c.method == "Restart")
        .unwrap_or(calls.len());
    let again = calls[restarted..]
        .iter()
        .filter(|c| c.method == "Prepare")
        .count();
    let mut wrong: Vec<String> = ended
        .iter()
        .filter(|(_, j)| j.state != js_blackbox::SUCCEEDED)
        .map(|(id, j)| {
            format!(
                "{id} did not succeed: {} {} {}",
                j.state, j.start_failure, j.start_failure_detail
            )
        })
        .collect();
    if again < WAITING {
        wrong.push(format!(
            "{again} Prepare calls reached the provider after its restart, fewer than the {WAITING} starts that waited"
        ));
    }
    verdict(&wrong, &calls, &ended)
}

/// `j5-outage`: catches a start that fails while the provider answers unavailable for a minute.
fn outage_body(rig: &Rig, jobs: &mut Vec<String>) -> Result<(), String> {
    rig.steady()?;
    let provider = rig.provider();
    provider.set(&format!("reset {MAX_PLACEMENTS} mount"))?;
    provider.set("unavailable")?;
    let ids = rig.submit_burst("j5-outage", WAITING, "blackbox-tree");
    jobs.extend(ids.iter().cloned());
    provider.wait_calls("Prepare", 1)?;
    watch(|w| w.hold(PROVIDER_OUTAGE, &mut || Ok(()))).kept("the provider's outage")?;
    provider.set("serve")?;
    let ended = rig.all_ended(&ids)?;
    let calls = provider.wait_calls("Release", WAITING)?;
    let wrong: Vec<String> = ended
        .iter()
        .filter(|(_, j)| j.state != js_blackbox::SUCCEEDED)
        .map(|(id, j)| {
            format!(
                "{id} did not succeed: {} {} {}",
                j.state, j.start_failure, j.start_failure_detail
            )
        })
        .collect();
    verdict(&wrong, &calls, &ended)
}

/// `j5-refused`: the boundary of "not the job's fault": a ref the provider refuses fails the
/// start as the job's own fault (`provider-refused`), and the agent asks once.
fn refused_body(rig: &Rig, jobs: &mut Vec<String>) -> Result<(), String> {
    rig.steady()?;
    let provider = rig.provider();
    provider.set(&format!("reset {MAX_PLACEMENTS} mount"))?;
    let ids = rig.submit_burst("j5-refused", 1, REFUSED_REF);
    jobs.extend(ids.iter().cloned());
    let ended = rig.all_ended(&ids)?;
    // Long enough for an agent that asks again to have asked (its first wait is about a second).
    watch(|w| w.hold(Duration::from_secs(10), &mut || Ok(()))).kept("the wait for a second ask")?;
    let calls = provider.calls()?;
    let asked = calls.iter().filter(|c| c.method == "Prepare").count();
    let mut wrong: Vec<String> = ended
        .iter()
        .filter(|(_, j)| {
            j.state != js_blackbox::FAILED_TO_START || j.start_failure_detail != PROVIDER_REFUSED
        })
        .map(|(id, j)| {
            format!(
                "{id} did not fail {PROVIDER_REFUSED}: {} {} {}",
                j.state, j.start_failure, j.start_failure_detail
            )
        })
        .collect();
    if asked != 1 {
        wrong.push(format!(
            "the refused ref was asked for {asked} times, not once"
        ));
    }
    verdict(&wrong, &calls, &ended)
}

/// `j5-burst`: see [`burst_body`].
pub(super) fn burst(rig: &Rig) -> Result<(), String> {
    guarded(rig, burst_body)
}

/// `j5-burst-at-prepare`: see [`burst_at_prepare_body`].
pub(super) fn burst_at_prepare(rig: &Rig) -> Result<(), String> {
    guarded(rig, burst_at_prepare_body)
}

/// `j5-restart`: see [`restart_body`].
pub(super) fn restart(rig: &Rig) -> Result<(), String> {
    guarded(rig, restart_body)
}

/// `j5-outage`: see [`outage_body`].
pub(super) fn outage(rig: &Rig) -> Result<(), String> {
    guarded(rig, outage_body)
}

/// `j5-refused`: see [`refused_body`].
pub(super) fn refused(rig: &Rig) -> Result<(), String> {
    guarded(rig, refused_body)
}
