//! A runner that answers like the `docker` command line, in memory: it keeps containers,
//! reads the arguments of `create`, `start`, `inspect`, `ps`, `stop`, `rm` and `info` the
//! way the real program does, and fails with an exit status where the real one would. It
//! records every call with the environment it was given. No process starts.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use js_agent::CommandRunner;

/// The version line of Docker.
pub const DOCKER: &str = "Docker version 27.3.1, build ce12230\n";

/// What a bind mount of a container is: source, destination, writable.
pub type Bind = (String, String, bool);

/// A volume of the local driver, as `volume inspect` reports it, and as `.Mounts` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub dst: String,
    pub driver: String,
    pub rw: bool,
    /// The driver options (`type`, `device`, `o`).
    pub options: BTreeMap<String, String>,
}

/// How a job's tampering changes the binds a container ends up with.
pub type Change = Box<dyn Fn(&mut Vec<Bind>)>;

/// How a job's tampering changes the volume a container ends up with.
pub type ChangeVolume = Box<dyn Fn(&mut Volume)>;

/// The environment of one call: `None` when the call was made with the agent's own
/// environment, the exact pairs otherwise.
pub type Env = Option<Vec<(String, String)>>;

struct Container {
    name: String,
    labels: BTreeMap<String, String>,
    create: Vec<String>,
    binds: Vec<Bind>,
    /// The name of the volume the container was made with, in [`State::volumes`].
    volume: Option<String>,
    started: bool,
    exit: Option<(i32, bool)>,
    /// The process id the engine reports while it runs.
    pid: u32,
}

#[derive(Default)]
struct State {
    version: String,
    containers: BTreeMap<String, Container>,
    next: u64,
    calls: Vec<Vec<String>>,
    envs: Vec<Env>,
    failing: BTreeMap<String, u32>,
    says: BTreeMap<String, String>,
    once: BTreeMap<String, String>,
    prefaces: BTreeMap<String, String>,
    after_create: Option<Result<String, ()>>,
    tamper: Option<Change>,
    tamper_volume: Option<ChangeVolume>,
    /// The volumes that exist, by name.
    volumes: BTreeMap<String, Volume>,
    /// What every look at the process of a container prints, instead of its pid and state.
    pid_says: Option<String>,
    /// How many more looks at the process of a container fail.
    pid_fails: u32,
    /// What the next looks at the process of a container print, one each, before the
    /// others: `None` is a call that exits with status 1.
    pid_queue: VecDeque<Option<String>>,
}

/// The runner. Clones share one world, so a test keeps a handle after it has handed the
/// runner to the runtime.
#[derive(Clone)]
pub struct Scripted(Rc<RefCell<State>>);

impl Scripted {
    /// A world with no container whose `--version` prints `version`.
    pub fn new(version: &str) -> Self {
        Self(Rc::new(RefCell::new(State {
            version: version.to_owned(),
            ..State::default()
        })))
    }

    /// Makes the next `times` calls of the subcommand `sub` exit with status 1.
    pub fn fail(&self, sub: &str, times: u32) {
        self.0.borrow_mut().failing.insert(sub.to_owned(), times);
    }

    /// Makes every call of `sub` print `text` and do nothing else.
    pub fn say(&self, sub: &str, text: &str) {
        self.0
            .borrow_mut()
            .says
            .insert(sub.to_owned(), text.to_owned());
    }

    /// Makes the next call of `sub` print `text` and do nothing else.
    pub fn say_once(&self, sub: &str, text: &str) {
        self.0
            .borrow_mut()
            .once
            .insert(sub.to_owned(), text.to_owned());
    }

    /// Makes every `sub` print `text` before what it prints (the progress of a pull).
    pub fn preface(&self, sub: &str, text: &str) {
        self.0
            .borrow_mut()
            .prefaces
            .insert(sub.to_owned(), text.to_owned());
    }

    /// Makes the next `create` make its container and then end as `outcome` says: `Err` is
    /// a nonzero exit (a call that was killed after the daemon had made the container),
    /// `Ok(text)` an exit 0 that printed `text` instead of the id.
    pub fn ambiguous_create(&self, outcome: Result<&str, ()>) {
        self.0.borrow_mut().after_create = Some(outcome.map(str::to_owned));
    }

    /// Makes every look at a container's process (`inspect` of `State.Pid`) print `text`.
    pub fn pid_says(&self, text: &str) {
        self.0.borrow_mut().pid_says = Some(text.to_owned());
    }

    /// Makes the next looks at a container's process (`inspect` of `State.Pid`) print
    /// `answers` in order (`None`: exit with status 1); later looks are answered as usual.
    pub fn pid_looks(&self, answers: &[Option<&str>]) {
        let queue = answers.iter().map(|a| a.map(str::to_owned)).collect();
        self.0.borrow_mut().pid_queue = queue;
    }

    /// Makes the next look at a container's process (`inspect` of `State.Pid`) exit with
    /// status 1; the other `inspect` calls are not touched.
    pub fn fail_pid(&self) {
        self.0.borrow_mut().pid_fails = 1;
    }

    /// Makes every `create` bind the container's mounts as `change` says, as a job that
    /// swapped a directory for a link while the daemon was setting up would.
    pub fn tamper(&self, change: impl Fn(&mut Vec<Bind>) + 'static) {
        self.0.borrow_mut().tamper = Some(Box::new(change));
    }

    /// Makes every `create` make its volume as `change` says, as an engine that bound the
    /// scratch directory otherwise than asked would.
    pub fn tamper_volume(&self, change: impl Fn(&mut Volume) + 'static) {
        self.0.borrow_mut().tamper_volume = Some(Box::new(change));
    }

    /// The names of the volumes that exist.
    pub fn volumes(&self) -> Vec<String> {
        self.0.borrow().volumes.keys().cloned().collect()
    }

    /// The volume of the container `id`, as the engine holds it.
    pub fn volume_of(&self, id: &str) -> Option<Volume> {
        let state = self.0.borrow();
        let name = state.containers.get(id)?.volume.clone()?;
        state.volumes.get(&name).cloned()
    }

    /// Every call, as the argv after the program, in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.0.borrow().calls.clone()
    }

    /// The environment of every call, in the order of [`Scripted::calls`].
    pub fn envs(&self) -> Vec<Env> {
        self.0.borrow().envs.clone()
    }

    /// The calls of one subcommand.
    pub fn calls_of(&self, sub: &str) -> Vec<Vec<String>> {
        let all = self.calls();
        all.into_iter()
            .filter(|args| args.first().map(String::as_str) == Some(sub))
            .collect()
    }

    /// The ids of the containers that exist.
    pub fn ids(&self) -> Vec<String> {
        self.0.borrow().containers.keys().cloned().collect()
    }

    /// The names of the containers that exist.
    pub fn names(&self) -> Vec<String> {
        let state = self.0.borrow();
        state.containers.values().map(|c| c.name.clone()).collect()
    }

    /// The id of the container whose run label is `run_id`.
    pub fn id_of(&self, run_id: &str) -> Option<String> {
        let state = self.0.borrow();
        let key = "jobscheduler.dev/run-id";
        let found = state
            .containers
            .iter()
            .find(|(_, c)| c.labels.get(key).map(String::as_str) == Some(run_id));
        found.map(|(id, _)| id.clone())
    }

    /// The `create` argv that made the container `id`.
    pub fn create_of(&self, id: &str) -> Vec<String> {
        self.0.borrow().containers[id].create.clone()
    }

    fn with_run(&self, run_id: &str, act: impl FnOnce(&mut Container)) {
        let id = self.id_of(run_id).expect("no such container");
        act(self.0.borrow_mut().containers.get_mut(&id).unwrap());
    }

    /// The container of `run_id` exits with `code`.
    pub fn exit(&self, run_id: &str, code: i32) {
        self.with_run(run_id, |c| c.exit = Some((code, false)));
    }

    /// The container of `run_id` is killed by the memory controller.
    pub fn exit_oom(&self, run_id: &str) {
        self.with_run(run_id, |c| c.exit = Some((137, true)));
    }

    /// The container of `run_id` disappears, as if removed from outside.
    pub fn vanish(&self, run_id: &str) {
        let id = self.id_of(run_id).expect("no such container");
        self.0.borrow_mut().containers.remove(&id);
    }

    /// Adds a container that was there before, running, with exactly these labels;
    /// returns its id.
    pub fn leak(&self, labels: &[(&str, &str)]) -> String {
        self.add(labels, true)
    }

    /// Adds a container that was created and never started, with exactly these labels;
    /// returns its id.
    pub fn leak_created(&self, labels: &[(&str, &str)]) -> String {
        self.add(labels, false)
    }

    /// Adds a running container called `name`, with exactly these labels: what an operator
    /// (or another agent) left on the host under a name the agent might choose; returns
    /// its id.
    pub fn leak_named(&self, name: &str, labels: &[(&str, &str)]) -> String {
        self.add_as(Some(name), labels, true)
    }

    fn add(&self, labels: &[(&str, &str)], started: bool) -> String {
        self.add_as(None, labels, started)
    }

    fn add_as(&self, name: Option<&str>, labels: &[(&str, &str)], started: bool) -> String {
        let mut state = self.0.borrow_mut();
        state.next += 1;
        let (id, name) = (
            format!("{:064x}", state.next),
            name.map_or_else(|| format!("leaked-{}", state.next), str::to_owned),
        );
        let labels = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        state.containers.insert(
            id.clone(),
            Container {
                name,
                labels,
                create: Vec::new(),
                binds: Vec::new(),
                volume: None,
                started,
                exit: None,
                pid: 4000,
            },
        );
        id
    }
}

fn failed(program: &str) -> Result<String, String> {
    Err(format!("{program}: exit status: 1"))
}

/// The values of every `flag` in `args`.
fn values<'a>(args: &'a [&str], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1])
        .collect()
}

/// Whether the engine's `name` filter `pattern` (a regular expression) matches `name`,
/// for the patterns the agent writes: anchored at both ends, `\.` a dot and a bare `.` any
/// one character. An unanchored pattern matches nothing here, so a test that sends one
/// finds no container.
fn name_matches(pattern: &str, name: &str) -> bool {
    let Some(body) = pattern
        .strip_prefix('^')
        .and_then(|rest| rest.strip_suffix('$'))
    else {
        return false;
    };
    let (mut wanted, mut have) = (body.chars(), name.chars());
    loop {
        match (wanted.next(), have.next()) {
            (None, None) => return true,
            (Some('\\'), Some(c)) if wanted.next() == Some(c) => {}
            (Some('.'), Some(_)) => {}
            (Some(p), Some(c)) if p == c => {}
            _ => return false,
        }
    }
}

/// The bind mounts a `create` asks for: `--mount type=bind,src=..,dst=..[,readonly,..]`.
fn binds_of(args: &[&str]) -> Vec<Bind> {
    values(args, "--mount")
        .into_iter()
        .filter(|value| value.starts_with("type=bind,"))
        .map(|value| {
            let part = |key: &str| {
                value
                    .split(',')
                    .find_map(|item| item.strip_prefix(key))
                    .unwrap_or_default()
                    .to_owned()
            };
            let readonly = value.split(',').any(|item| item == "readonly");
            (part("src="), part("dst="), !readonly)
        })
        .collect()
}

/// The fields of a CSV record, as the engine reads a `--mount` value: a field in double
/// quotes may hold commas.
fn csv_fields(value: &str) -> Vec<String> {
    let (mut fields, mut field, mut quoted) = (Vec::new(), String::new(), false);
    for c in value.chars() {
        match (c, quoted) {
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

/// The volume a `--mount type=volume,..` value asks for, when it is one.
fn volume_of_mount(args: &[&str]) -> Option<Volume> {
    let value = values(args, "--mount")
        .into_iter()
        .find(|value| value.starts_with("type=volume,"))?;
    let fields = csv_fields(value);
    let field = |key: &str| {
        fields
            .iter()
            .find_map(|item| item.strip_prefix(key))
            .unwrap_or_default()
            .to_owned()
    };
    let options = fields
        .iter()
        .filter_map(|item| item.strip_prefix("volume-opt="))
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Some(Volume {
        dst: field("dst="),
        driver: field("volume-driver="),
        rw: true,
        options,
    })
}

impl State {
    fn resolve(&self, key: &str) -> Option<String> {
        let by_name = self.containers.iter().find(|(_, c)| c.name == key);
        self.containers
            .contains_key(key)
            .then(|| key.to_owned())
            .or_else(|| by_name.map(|(id, _)| id.clone()))
    }

    fn create(&mut self, args: &[&str]) -> String {
        self.next += 1;
        let id = format!("{:064x}", self.next);
        let labels = values(args, "--label")
            .into_iter()
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let create = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut binds = binds_of(args);
        if let Some(change) = &self.tamper {
            change(&mut binds);
        }
        let name = values(args, "--name")
            .first()
            .map_or_else(|| format!("auto-{}", self.next), |name| (*name).to_owned());
        let volume = volume_of_mount(args).map(|mut volume| {
            if let Some(change) = &self.tamper_volume {
                change(&mut volume);
            }
            let volume_name = format!("{:064x}", 0xfeed_u64 + self.next);
            self.volumes.insert(volume_name.clone(), volume);
            volume_name
        });
        let container = Container {
            name,
            labels,
            create,
            binds,
            volume,
            started: false,
            exit: None,
            pid: 4000,
        };
        self.containers.insert(id.clone(), container);
        format!("{id}\n")
    }

    fn mounts(&self, container: &Container) -> String {
        let volume = container
            .volume
            .as_ref()
            .and_then(|name| Some((name.clone(), self.volumes.get(name)?.clone())));
        let mut mounts: Vec<serde_json::Value> = container
            .binds
            .iter()
            .map(|(src, dst, rw)| {
                serde_json::json!({
                    "Type": "bind", "Source": src, "Destination": dst,
                    "Mode": "", "RW": rw, "Propagation": "rprivate"
                })
            })
            .collect();
        mounts.extend(volume.map(|(name, v)| {
            serde_json::json!({
                "Type": "volume", "Name": name, "Driver": v.driver,
                "Source": format!("/var/lib/engine/volumes/{name}/_data"),
                "Destination": v.dst, "Mode": "z", "RW": v.rw, "Propagation": ""
            })
        }));
        mounts.push(serde_json::json!({
            "Type": "tmpfs", "Source": "", "Destination": "/tmp", "Mode": "", "RW": true
        }));
        serde_json::to_string(&mounts).unwrap()
    }

    fn inspect(&self, args: &[&str], container: &Container) -> String {
        let format = values(args, "--format")[0];
        let state = match (container.started, container.exit) {
            (false, _) => "created",
            (true, None) => "running",
            (true, Some(_)) => "exited",
        };
        if format.contains("State.Pid") {
            // A container that is not running has no process.
            let pid = if state == "running" { container.pid } else { 0 };
            return self
                .pid_says
                .clone()
                .unwrap_or_else(|| format!("{pid} {state}\n"));
        }
        let labels = serde_json::to_string(&container.labels).unwrap();
        match (format.contains("Mounts"), format.contains("Config.Labels")) {
            (true, _) => format!("{}\n", self.mounts(container)),
            (false, true) => format!("{state} {labels}\n"),
            (false, false) => match container.exit {
                None => format!("{state} 0 false\n"),
                Some((code, oom)) => format!("exited {code} {oom}\n"),
            },
        }
    }

    fn ps(&self, args: &[&str]) -> String {
        let wanted = values(args, "--filter");
        let matches = |id: &str, c: &Container| {
            wanted.iter().all(|filter| match filter.split_once('=') {
                Some(("id", prefix)) => id.starts_with(prefix),
                Some(("name", pattern)) => name_matches(pattern, &c.name),
                Some(("label", rest)) => match rest.split_once('=') {
                    Some((k, v)) => c.labels.get(k).map(String::as_str) == Some(v),
                    None => c.labels.contains_key(rest),
                },
                _ => false,
            })
        };
        self.containers
            .iter()
            .filter(|(id, c)| matches(id, c))
            .map(|(id, _)| format!("{id}\n"))
            .collect()
    }
}

impl Scripted {
    fn exec(&self, program: &str, args: &[&str], env: Env) -> Result<String, String> {
        let mut state = self.0.borrow_mut();
        state
            .calls
            .push(args.iter().map(|a| (*a).to_owned()).collect());
        state.envs.push(env);
        let sub = args[0];
        let looks_at_pid = sub == "inspect" && values(args, "--format")[0].contains("State.Pid");
        if looks_at_pid && state.pid_fails > 0 {
            state.pid_fails -= 1;
            return failed(program);
        }
        if let Some(answer) = looks_at_pid.then(|| state.pid_queue.pop_front()).flatten() {
            return answer.ok_or_else(|| format!("{program}: exit status: 1"));
        }
        if let Some(left) = state.failing.get_mut(sub).filter(|left| **left > 0) {
            *left -= 1;
            return failed(program);
        }
        if sub == "--version" {
            return Ok(state.version.clone());
        }
        if let Some(text) = state.once.remove(sub) {
            return Ok(text);
        }
        if let Some(text) = state.says.get(sub) {
            return Ok(text.clone());
        }
        let key = args.last().copied().unwrap_or_default();
        let id = state.resolve(key).unwrap_or_default();
        let known = state.containers.contains_key(&id);
        match (sub, known) {
            ("create", _) => {
                let taken = values(args, "--name")
                    .first()
                    .is_some_and(|name| state.resolve(name).is_some());
                if taken {
                    return failed(program);
                }
                let preface = state.prefaces.get(sub).cloned().unwrap_or_default();
                let printed = state.create(args);
                match state.after_create.take() {
                    Some(Err(())) => failed(program),
                    Some(Ok(text)) => Ok(text),
                    None => Ok(format!("{preface}{printed}")),
                }
            }
            ("ps", _) => Ok(state.ps(args)),
            ("volume", _) => match (args.get(1), state.volumes.get(key)) {
                (Some(&"inspect"), Some(volume)) => Ok(format!(
                    "{}\n",
                    serde_json::to_string(&volume.options).unwrap()
                )),
                _ => failed(program),
            },
            ("info", _) => Ok("true\n".to_owned()),
            // The log of a container the world knows: empty unless a test `say`s it.
            ("logs", true) => Ok(String::new()),
            ("start", true) => {
                state.containers.get_mut(&id).unwrap().started = true;
                Ok(format!("{id}\n"))
            }
            ("inspect", true) => Ok(state.inspect(args, &state.containers[&id])),
            ("stop", true) => {
                let container = state.containers.get_mut(&id).unwrap();
                container.exit = container.exit.or(Some((137, false)));
                Ok(format!("{id}\n"))
            }
            ("rm", true) => {
                let gone = state.containers.remove(&id);
                if let Some(name) = gone
                    .and_then(|c| c.volume)
                    .filter(|_| args.contains(&"--volumes"))
                {
                    state.volumes.remove(&name);
                }
                Ok(format!("{id}\n"))
            }
            _ => failed(program),
        }
    }
}

impl CommandRunner for Scripted {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.exec(program, args, None)
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.exec(program, args, Some(env))
    }

    /// Only `logs` is read this way: what the world prints is its standard output, and its
    /// standard error is empty.
    fn run_streams(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        _keep: usize,
        _limit: std::time::Duration,
    ) -> Result<js_agent::Streams, String> {
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.exec(program, args, Some(env))
            .map(|text| js_agent::Streams {
                stdout: text.into_bytes(),
                ..js_agent::Streams::default()
            })
    }
}
