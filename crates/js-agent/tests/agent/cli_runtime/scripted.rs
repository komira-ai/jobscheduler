//! A runner that answers like the `docker` command line, in memory: it keeps containers,
//! reads the arguments of `create`, `start`, `inspect`, `ps`, `stop` and `rm` the way the
//! real program does, and fails with an exit status where the real one would. It records
//! every call. No process starts.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use js_agent::CommandRunner;

/// The version line of Docker.
pub const DOCKER: &str = "Docker version 27.3.1, build ce12230\n";

struct Container {
    labels: BTreeMap<String, String>,
    create: Vec<String>,
    exit: Option<(i32, bool)>,
}

#[derive(Default)]
struct State {
    version: String,
    containers: BTreeMap<String, Container>,
    next: u64,
    calls: Vec<Vec<String>>,
    failing: BTreeMap<String, u32>,
    says: BTreeMap<String, String>,
    once: BTreeMap<String, String>,
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

    /// Every call, as the argv after the program, in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.0.borrow().calls.clone()
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

    /// Adds a container that was there before, with exactly these labels; returns its id.
    pub fn leak(&self, labels: &[(&str, &str)]) -> String {
        let mut state = self.0.borrow_mut();
        state.next += 1;
        let id = format!("{:064x}", state.next);
        let labels = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        state.containers.insert(
            id.clone(),
            Container {
                labels,
                create: Vec::new(),
                exit: None,
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

impl State {
    fn create(&mut self, args: &[&str]) -> String {
        self.next += 1;
        let id = format!("{:064x}", self.next);
        let labels = values(args, "--label")
            .into_iter()
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let create = args.iter().map(|arg| (*arg).to_owned()).collect();
        let container = Container {
            labels,
            create,
            exit: None,
        };
        self.containers.insert(id.clone(), container);
        format!("{id}\n")
    }

    fn inspect(&self, args: &[&str], container: &Container) -> String {
        let format = values(args, "--format")[0];
        match (format.contains("Config.Labels"), container.exit) {
            (true, _) => {
                let json = serde_json::to_string(&container.labels).unwrap();
                format!("{json}\n")
            }
            (false, None) => "running 0 false\n".to_owned(),
            (false, Some((code, oom))) => format!("exited {code} {oom}\n"),
        }
    }

    fn ps(&self, args: &[&str]) -> String {
        let wanted = values(args, "--filter");
        let matches = |id: &str, c: &Container| {
            wanted.iter().all(|filter| match filter.split_once('=') {
                Some(("id", prefix)) => id.starts_with(prefix),
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

impl CommandRunner for Scripted {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let mut state = self.0.borrow_mut();
        state
            .calls
            .push(args.iter().map(|a| (*a).to_owned()).collect());
        let sub = args[0];
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
        let id = args.last().copied().unwrap_or_default();
        let known = state.containers.contains_key(id);
        match (sub, known) {
            ("create", _) => Ok(state.create(args)),
            ("ps", _) => Ok(state.ps(args)),
            ("start", true) => Ok(format!("{id}\n")),
            ("inspect", true) => Ok(state.inspect(args, &state.containers[id])),
            ("stop", true) => {
                let container = state.containers.get_mut(id).unwrap();
                container.exit = container.exit.or(Some((137, false)));
                Ok(format!("{id}\n"))
            }
            ("rm", true) => {
                state.containers.remove(id);
                Ok(format!("{id}\n"))
            }
            _ => failed(program),
        }
    }
}
