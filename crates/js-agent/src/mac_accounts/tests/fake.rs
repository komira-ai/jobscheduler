//! A host kept in memory: users, processes, launchd domains and IPC objects, with a counter
//! clock, a log of the calls made, failures to inject and hooks to run when a call is made.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::super::generated_uid;
use super::super::host::{
    AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord,
};

/// What `launchctl print` says of a domain.
#[derive(Clone, Copy, Debug)]
pub(in super::super) enum Print {
    /// Exit 0, with a service in it: the domain stays.
    Stays,
    /// Exit 0, with nothing in it: `user/<uid>` of a uid with no domain.
    Empty,
    /// Ended by a signal: no answer.
    Signal,
}

/// An IPC object `ipcs -a` lists.
#[derive(Clone, Debug)]
pub(in super::super) struct Ipc {
    pub(in super::super) kind: char,
    pub(in super::super) id: u64,
    pub(in super::super) owner: String,
    pub(in super::super) creator: String,
}

/// What the fake keeps.
#[derive(Default)]
pub(in super::super) struct State {
    pub(in super::super) users: BTreeMap<String, NewUser>,
    pub(in super::super) processes: BTreeMap<u32, Vec<LiveProcess>>,
    /// Uids whose processes survive `kill_all`.
    pub(in super::super) immortal: BTreeSet<u32>,
    /// What `launchctl print` says of a domain; a domain not here is not found (exit 113).
    pub(in super::super) print: BTreeMap<String, Print>,
    /// Domains `bootout` does not end.
    pub(in super::super) stubborn: BTreeSet<String>,
    pub(in super::super) ipc: Vec<Ipc>,
    /// Whether `ipcrm` does nothing (and says so).
    pub(in super::super) ipcrm_fails: bool,
    pub(in super::super) clock: u64,
    pub(in super::super) pauses: u64,
    /// The calls made, `<op> <argument>`.
    pub(in super::super) calls: Vec<String>,
    /// Calls that fail: an operation (`kill_all`) or one call (`kill_all 600`).
    pub(in super::super) failing: BTreeSet<String>,
    /// How the grant answers: `None` is held.
    pub(in super::super) grant: Option<io::ErrorKind>,
    /// Whether `list_accounts` ignores its prefix.
    pub(in super::super) list_ignores_prefix: bool,
    /// The `GeneratedUID` every record is made with, instead of the one it is asked for: a
    /// directory that did not keep what it was given.
    pub(in super::super) made_with_guid: Option<String>,
    /// Whether `ipcs` says it failed (exit status 1).
    pub(in super::super) ipcs_exits_nonzero: bool,
    /// How much each operation advances the clock.
    pub(in super::super) cost: BTreeMap<&'static str, u64>,
}

type Hook = Arc<Mutex<dyn FnMut() + Send>>;

/// The host. Shared by the pool and the test.
#[derive(Default)]
pub(in super::super) struct Fake {
    state: Mutex<State>,
    hooks: Mutex<Vec<(String, Hook)>>,
}

fn locked<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fake {
    pub(in super::super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The state, locked: do not call the host while holding it.
    pub(in super::super) fn state(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    /// Makes the call `key` (an operation, or `<op> <argument>`) fail until [`Fake::heal`].
    pub(in super::super) fn fail(&self, key: &str) {
        self.state().failing.insert(key.to_owned());
    }

    /// Lets the call `key` work again.
    pub(in super::super) fn heal(&self, key: &str) {
        self.state().failing.remove(key);
    }

    /// Runs `hook` after every call that is `key` (an operation, or `<op> <argument>`), once
    /// the call has done its work and before it returns.
    pub(in super::super) fn on(&self, key: &str, hook: impl FnMut() + Send + 'static) {
        locked(&self.hooks).push((key.to_owned(), Arc::new(Mutex::new(hook))));
    }

    /// The calls made so far, and forgets them.
    pub(in super::super) fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.state().calls)
    }

    /// A user record, as `create_account` makes it.
    pub(in super::super) fn add_user(&self, name: &str, uid: u32, gid: u32, homes: &Path) {
        let user = NewUser {
            name: name.to_owned(),
            uid,
            gid,
            home: homes.join(name),
            generated_uid: generated_uid(uid),
        };
        self.state().users.insert(name.to_owned(), user);
    }

    /// A process of `uid`.
    pub(in super::super) fn add_process(&self, uid: u32, pid: i32, command: &str) {
        self.state()
            .processes
            .entry(uid)
            .or_default()
            .push(LiveProcess {
                pid,
                command: command.to_owned(),
            });
    }

    /// An IPC object of `owner`, created by `creator`.
    pub(in super::super) fn add_ipc(&self, kind: char, id: u64, owner: &str, creator: &str) {
        self.state().ipc.push(Ipc {
            kind,
            id,
            owner: owner.to_owned(),
            creator: creator.to_owned(),
        });
    }

    /// Logs the call, fails it when told to, advances the clock by its cost, and returns the
    /// hooks to run once it has done its work.
    fn enter(&self, op: &'static str, arg: &str) -> io::Result<Vec<Hook>> {
        let key = format!("{op} {arg}").trim_end().to_owned();
        let mut state = self.state();
        state.calls.push(key.clone());
        state.clock += state.cost.get(op).copied().unwrap_or(0);
        if state.failing.contains(op) || state.failing.contains(&key) {
            return Err(io::Error::other(format!("{key} failed")));
        }
        drop(state);
        Ok(locked(&self.hooks)
            .iter()
            .filter(|(wanted, _)| wanted == op || *wanted == key)
            .map(|(_, hook)| Arc::clone(hook))
            .collect())
    }

    /// Runs the hooks `enter` returned.
    fn after<T>(hooks: Vec<Hook>, answer: T) -> io::Result<T> {
        for hook in hooks {
            (locked(&hook))();
        }
        Ok(answer)
    }

    /// `ipcs -a`'s text.
    fn ipcs(&self) -> String {
        let mut text = String::from(
            "IPC status from <running system> as of Thu Nov  5 10:00:00 2026\n\
             T     ID     KEY        MODE       OWNER    GROUP\n",
        );
        for object in &self.state().ipc {
            text.push_str(&format!(
                "{}  {} 0x00000001 --rw-------  {}    staff {} staff\n",
                object.kind, object.id, object.owner, object.creator
            ));
        }
        text
    }
}

impl AccountHost for Fake {
    fn full_disk_access(&self) -> io::Result<()> {
        let hooks = self.enter("full_disk_access", "")?;
        let grant = self.state().grant;
        match grant {
            None => Self::after(hooks, ()),
            Some(kind) => Err(io::Error::new(
                kind,
                "the privacy directory cannot be listed",
            )),
        }
    }

    fn account(&self, name: &str) -> io::Result<Option<UserRecord>> {
        let hooks = self.enter("account", name)?;
        let found = self.state().users.get(name).map(|user| UserRecord {
            uid: user.uid,
            gid: user.gid,
        });
        Self::after(hooks, found)
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        let hooks = self.enter("uid_taken", &uid.to_string())?;
        let taken = self.state().users.values().any(|user| user.uid == uid);
        Self::after(hooks, taken)
    }

    fn guid_taken(&self, guid: &str) -> io::Result<bool> {
        let hooks = self.enter("guid_taken", guid)?;
        let taken = self
            .state()
            .users
            .values()
            .any(|user| user.generated_uid == guid);
        Self::after(hooks, taken)
    }

    fn record_guid(&self, name: &str) -> io::Result<Option<String>> {
        let hooks = self.enter("record_guid", name)?;
        let found = self
            .state()
            .users
            .get(name)
            .map(|user| user.generated_uid.clone());
        Self::after(hooks, found)
    }

    fn list_accounts(&self, prefix: &str) -> io::Result<Vec<(String, u32)>> {
        let hooks = self.enter("list_accounts", prefix)?;
        let state = self.state();
        let found = state
            .users
            .values()
            .filter(|user| state.list_ignores_prefix || user.name.starts_with(prefix))
            .map(|user| (user.name.clone(), user.uid))
            .collect();
        drop(state);
        Self::after(hooks, found)
    }

    fn make_home(&self, homes: &Path, name: &str, _uid: u32, _gid: u32) -> io::Result<()> {
        let hooks = self.enter("make_home", name)?;
        std::fs::create_dir(homes.join(name))?;
        Self::after(hooks, ())
    }

    fn create_account(&self, user: &NewUser) -> io::Result<()> {
        let hooks = self.enter("create_account", &user.name)?;
        let mut state = self.state();
        if state.users.contains_key(&user.name) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let mut user = user.clone();
        if let Some(guid) = state.made_with_guid.clone() {
            user.generated_uid = guid;
        }
        state.users.insert(user.name.clone(), user);
        drop(state);
        Self::after(hooks, ())
    }

    fn delete_account(&self, name: &str) -> io::Result<bool> {
        let hooks = self.enter("delete_account", name)?;
        let was = self.state().users.remove(name).is_some();
        Self::after(hooks, was)
    }

    fn processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>> {
        let hooks = self.enter("processes", &uid.to_string())?;
        let found = self
            .state()
            .processes
            .get(&uid)
            .cloned()
            .unwrap_or_default();
        Self::after(hooks, found)
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        let hooks = self.enter("kill_all", &uid.to_string())?;
        let mut state = self.state();
        if !state.immortal.contains(&uid) {
            state.processes.remove(&uid);
        }
        drop(state);
        Self::after(hooks, ())
    }

    fn launchctl_print(&self, domain: &str) -> io::Result<PrintAnswer> {
        let hooks = self.enter("launchctl_print", domain)?;
        let (code, stdout) = match self.state().print.get(domain) {
            None => (Some(113), String::new()),
            Some(Print::Signal) => (None, String::new()),
            Some(Print::Empty) => (
                Some(0),
                "system = {\n\tservices = {\n\t}\n\tunmanaged processes = {\n\t}\n}\n".to_owned(),
            ),
            Some(Print::Stays) => (
                Some(0),
                "system = {\n\tservices = {\n\t\t123 - com.example.agent\n\t}\n\tunmanaged processes = {\n\t}\n}\n"
                    .to_owned(),
            ),
        };
        Self::after(
            hooks,
            PrintAnswer {
                code,
                stdout,
                stderr: "Could not find service".to_owned(),
            },
        )
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        let hooks = self.enter("bootout", domain)?;
        let mut state = self.state();
        let ended = state.print.contains_key(domain) && !state.stubborn.contains(domain);
        if ended {
            state.print.remove(domain);
        }
        drop(state);
        match ended {
            true => Self::after(hooks, ()),
            false => Err(io::Error::other("Boot-out failed: 5: Input/output error")),
        }
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let hooks = self.enter("command", &format!("{program} {}", args.join(" ")))?;
        let output = match (program, args) {
            ("/usr/bin/ipcs", ["-a"]) => {
                let nonzero = self.state().ipcs_exits_nonzero;
                CommandOutput {
                    success: !nonzero,
                    stdout: self.ipcs(),
                    stderr: "ipcs: kernel is not answering ".to_owned(),
                }
            }
            ("/usr/bin/ipcrm", [kind, id]) => {
                let mut state = self.state();
                let fails = state.ipcrm_fails;
                if !fails {
                    state.ipc.retain(|object| {
                        format!("-{}", object.kind) != *kind || object.id.to_string() != *id
                    });
                }
                CommandOutput {
                    success: !fails,
                    stdout: String::new(),
                    stderr: "ipcrm: not permitted".to_owned(),
                }
            }
            _ => return Err(io::Error::other(format!("no such program: {program}"))),
        };
        Self::after(hooks, output)
    }

    fn pause(&self) {
        let mut state = self.state();
        state.pauses += 1;
        state.clock += 100;
    }

    fn now_ms(&self) -> u64 {
        self.state().clock
    }
}
