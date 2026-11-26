//! What the spy answers to the unit manager's own command line (`systemctl`), by its arguments:
//! the version, the signal that asks an agent to hand its runs over in place, and the reading of
//! the unit's tasks.

use js_agent::update_hook::StateView;

use super::Spy;

/// The version `systemctl --version` prints by default: one older than the first that starts a
/// service's first process in the unit's own cgroup, so that a hand-over by stop and start is
/// allowed.
pub const SYSTEMD_257: &str = "systemd 257 (257.9)\n+PAM +AUDIT\n";

/// What a version 258 prints.
pub const SYSTEMD_258: &str = "systemd 258 (258.2)\n+PAM +AUDIT\n";

/// What the agent does when the signal of the request reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnSignal {
    /// It hands over, removes the request and runs its program again as the same process: the
    /// unit stays active and the host sees a new session.
    Takes,
    /// It does nothing: the request stays.
    Ignores,
    /// It hands over and ends instead of running again: the request is gone and the unit is
    /// `inactive`.
    Ends,
}

impl Spy<'_> {
    /// The answer to a `systemctl` run: `--version` prints [`Spy::systemd`]; `kill` is an act
    /// (it fails when the call `kill` is set to fail, leaving the unit as
    /// [`Spy::kill_refused_leaves`] says) and the agent reacts as [`Spy::on_signal`] says;
    /// anything else, the reading of the unit's tasks, takes the next answer of
    /// [`Spy::tasks`], and `0` once they are used up.
    pub(super) fn systemctl(&self, args: &[&str]) -> Result<String, String> {
        match args.first().copied() {
            Some("--version") => self.systemd.borrow().clone(),
            Some("kill") => {
                self.act(format!("systemctl.{}", args.join(" ")));
                if self.fail.borrow().contains("kill") {
                    if let Some((active, sub)) = *self.kill_refused_leaves.borrow() {
                        let unit = args.last().copied().unwrap_or_default();
                        self.world.seed_unit(unit, active, sub);
                    }
                    return Err("kill is down".to_owned());
                }
                self.kill
                    .as_agent(|| self.agent_reacts(args.last().copied().unwrap_or_default()));
                Ok(String::new())
            }
            _ => self
                .tasks
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok("0\n".to_owned())),
        }
    }

    fn agent_reacts(&self, unit: &str) {
        match self.on_signal.get() {
            OnSignal::Ignores => {}
            OnSignal::Takes => {
                StateView::withdraw_handover(&self.state).unwrap();
                self.world.hello_again().unwrap();
            }
            OnSignal::Ends => {
                StateView::withdraw_handover(&self.state).unwrap();
                self.world.seed_unit(unit, "inactive", "dead");
            }
        }
    }
}
