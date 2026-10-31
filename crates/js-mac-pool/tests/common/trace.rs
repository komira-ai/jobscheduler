//! The pure half of the base-set trace (`trace_base_set.rs`): which profile line to take
//! out, how a workload's argv is rewritten, what a row of the table is, how `fs_usage`
//! output is read, and the loop that runs every workload with each line removed in turn.
//! Nothing here touches the host; the runner is a closure the caller passes.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use js_mac_pool::seatbelt::{BaseSet, Match, Outcome};

/// Why a trace step could not be done.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    /// The profile has no line of the base set in the form the generator writes it.
    #[error("the profile has no line {0:?}")]
    LineNotFound(String),
    /// A workload's argv is not `sandbox-exec ... -p <profile> program args`.
    #[error("workload {0} is not run under a profile")]
    NoProfile(String),
    /// A table row is not in the table format.
    #[error("bad table row: {0:?}")]
    BadRow(String),
}

/// What a row of the table is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// What a workload did with the whole base set.
    Baseline,
    /// A `file-read-metadata` name of the base set.
    Name,
    /// A `file-read*` line of the base set.
    Read,
    /// A setuid or setgid file of the host that the traced set lists.
    Setuid,
    /// A setuid or setgid file of the host that the traced set does not list.
    SetuidNew,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Name => "name",
            Self::Read => "read",
            Self::Setuid => "setuid",
            Self::SetuidNew => "setuid-new",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [
            Self::Baseline,
            Self::Name,
            Self::Read,
            Self::Setuid,
            Self::SetuidNew,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
    }
}

/// A yes/no column that may also be unknown or not applicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Yes.
    Yes,
    /// No.
    No,
    /// Not measured.
    Unknown,
    /// Not a question for this row.
    Na,
}

impl Mark {
    fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "?",
            Self::Na => "-",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Yes, Self::No, Self::Unknown, Self::Na]
            .into_iter()
            .find(|mark| mark.as_str() == text)
    }
}

/// One row of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// What the row is about.
    pub kind: Kind,
    /// The base-set line (`literal /etc`, `subpath /usr/lib`), the workload or the file.
    pub line: String,
    /// Whether a workload touched the line (from `fs_usage`).
    pub touched: Mark,
    /// Whether a workload fails without the line.
    pub needed: Mark,
    /// Baseline: the outcome. Otherwise the workloads that fail without the line.
    pub workloads: Vec<String>,
    /// `workload/process` pairs that touched the line.
    pub touched_by: Vec<String>,
    /// Denied reads whose outcome changed without the line (expected: none).
    pub denial_changed: Vec<String>,
}

impl Row {
    /// A row with nothing measured.
    #[must_use]
    pub fn new(kind: Kind, line: &str) -> Self {
        Self {
            kind,
            line: line.to_owned(),
            touched: Mark::Na,
            needed: Mark::Na,
            workloads: Vec::new(),
            touched_by: Vec::new(),
            denial_changed: Vec::new(),
        }
    }

    /// What `name` did with the whole base set.
    #[must_use]
    pub fn baseline(name: &str, outcome: Outcome) -> Self {
        let mut row = Self::new(Kind::Baseline, name);
        row.workloads.push(outcome_str(outcome).to_owned());
        row
    }
}

/// The outcome as the table spells it.
#[must_use]
pub fn outcome_str(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Success => "success",
        Outcome::Refused => "refused",
        Outcome::Failed => "failed",
    }
}

const HEADER: &str = "kind\tline\ttouched\tneeded\tworkloads\ttouched_by\tdenial_changed";

fn join(list: &[String]) -> String {
    if list.is_empty() {
        return "-".to_owned();
    }
    list.join(",")
}

fn split(text: &str) -> Vec<String> {
    if text == "-" {
        return Vec::new();
    }
    text.split(',').map(str::to_owned).collect()
}

/// The table as text: `# key=value` lines, the header, then one tab-separated row each.
#[must_use]
pub fn render(meta: &[(String, String)], rows: &[Row]) -> String {
    let mut out = String::new();
    for (key, value) in meta {
        out.push_str(&format!("# {key}={value}\n"));
    }
    out.push_str(HEADER);
    out.push('\n');
    for row in rows {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.kind.as_str(),
            row.line,
            row.touched.as_str(),
            row.needed.as_str(),
            join(&row.workloads),
            join(&row.touched_by),
            join(&row.denial_changed)
        ));
    }
    out
}

/// The rows of a table written by [`render`] (the `#` lines and the header are skipped).
///
/// # Errors
/// [`TraceError::BadRow`] for a row that is not seven columns of known values.
pub fn parse(text: &str) -> Result<Vec<Row>, TraceError> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line == HEADER {
            continue;
        }
        let bad = || TraceError::BadRow(line.to_owned());
        let cols: Vec<&str> = line.split('\t').collect();
        let [kind, name, touched, needed, workloads, by, denial] = cols[..] else {
            return Err(bad());
        };
        rows.push(Row {
            kind: Kind::parse(kind).ok_or_else(bad)?,
            line: name.to_owned(),
            touched: Mark::parse(touched).ok_or_else(bad)?,
            needed: Mark::parse(needed).ok_or_else(bad)?,
            workloads: split(workloads),
            touched_by: split(by),
            denial_changed: split(denial),
        });
    }
    Ok(rows)
}

/// One line of a base set: a name or a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// A name (metadata only) or a read.
    pub kind: Kind,
    /// How the path is matched.
    pub how: Match,
    /// The path.
    pub path: String,
}

impl Line {
    /// The label of the table: `literal /etc`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {}", self.word(), self.path)
    }

    fn word(&self) -> &'static str {
        match self.how {
            Match::Literal => "literal",
            Match::Subpath => "subpath",
        }
    }

    /// The filter as the profile spells it.
    fn filter(&self) -> String {
        format!("({} \"{}\")", self.word(), self.path)
    }
}

/// The names and then the reads of `base`.
#[must_use]
pub fn lines(base: &BaseSet) -> Vec<Line> {
    let names = base.names().iter().map(|path| Line {
        kind: Kind::Name,
        how: Match::Literal,
        path: (*path).to_owned(),
    });
    let reads = base.reads().iter().map(|(how, path)| Line {
        kind: Kind::Read,
        how: *how,
        path: (*path).to_owned(),
    });
    names.chain(reads).collect()
}

fn joined(lines: &[&Line]) -> String {
    let each: Vec<String> = lines.iter().map(|line| line.filter()).collect();
    each.join(" ")
}

/// `profile` with `line` taken out of the base set's own rule: the names rule
/// (`file-read-metadata`) or the reads rule (`file-read*`). The same filter in another
/// rule (`/dev/null` in the write grant, say) stays.
///
/// # Errors
/// [`TraceError::LineNotFound`] when the profile has no such rule, so a change of the
/// generator's format cannot pass for an ablation that removed nothing.
pub fn without(profile: &str, base: &BaseSet, line: &Line) -> Result<String, TraceError> {
    let all = lines(base);
    let (verb, same): (&str, Vec<&Line>) = match line.kind {
        Kind::Name => (
            "file-read-metadata",
            all.iter().filter(|l| l.kind == Kind::Name).collect(),
        ),
        _ => (
            "file-read*",
            all.iter().filter(|l| l.kind == Kind::Read).collect(),
        ),
    };
    let whole = format!("(allow {verb} {})", joined(&same));
    let kept: Vec<&Line> = same.iter().copied().filter(|l| *l != line).collect();
    let rest = if kept.is_empty() {
        String::new()
    } else {
        format!("(allow {verb} {})", joined(&kept))
    };
    let at = profile
        .lines()
        .position(|text| text == whole)
        .ok_or_else(|| TraceError::LineNotFound(whole.clone()))?;
    let mut out: Vec<&str> = profile.lines().collect();
    out[at] = &rest;
    if rest.is_empty() {
        out.remove(at);
    }
    let mut text = out.join("\n");
    text.push('\n');
    Ok(text)
}

/// The index of the profile text in a `sandbox-exec ... -p <profile> program args` argv.
fn profile_at(argv: &[OsString]) -> Option<usize> {
    let flag = argv.iter().position(|arg| arg == "-p")?;
    (flag + 2 < argv.len()).then_some(flag + 1)
}

/// The profile text of a sandboxed argv.
#[must_use]
pub fn profile_of(argv: &[OsString]) -> Option<String> {
    let at = profile_at(argv)?;
    Some(argv[at].to_string_lossy().into_owned())
}

/// `argv` with another profile text.
#[must_use]
pub fn with_profile(argv: &[OsString], text: &str) -> Option<Vec<OsString>> {
    let at = profile_at(argv)?;
    let mut out = argv.to_vec();
    out[at] = OsString::from(text);
    Some(out)
}

/// `argv`'s `sandbox-exec` and profile with another program and arguments.
#[must_use]
pub fn retarget(argv: &[OsString], program: &str, args: &[OsString]) -> Option<Vec<OsString>> {
    let at = profile_at(argv)?;
    let mut out = argv[..=at].to_vec();
    out.push(OsString::from(program));
    out.extend(args.iter().cloned());
    Some(out)
}

/// A command to run under a profile.
#[derive(Debug, Clone)]
pub struct Workload {
    /// The name in the table.
    pub name: String,
    /// Allowed workloads must succeed; denied ones must be refused.
    pub allowed: bool,
    /// `sandbox-exec ... -p <profile> program args`.
    pub argv: Vec<OsString>,
    /// The environment.
    pub env: Vec<(String, String)>,
    /// The working directory.
    pub cwd: String,
}

/// Runs every workload with the whole base set, then with each line of it removed in
/// turn, through `run`, and returns the baseline rows and one row per line. A line is
/// needed when an allowed workload that succeeds with the whole set does not without it;
/// a denied workload whose outcome changes is listed too (it should not).
///
/// # Errors
/// [`TraceError::NoProfile`] and [`TraceError::LineNotFound`].
pub fn ablate(
    base: &BaseSet,
    workloads: &[Workload],
    run: &mut dyn FnMut(&Workload, &[OsString]) -> Outcome,
) -> Result<Vec<Row>, TraceError> {
    let mut rows = Vec::new();
    let mut whole = Vec::new();
    for work in workloads {
        let outcome = run(work, &work.argv);
        whole.push(outcome);
        rows.push(Row::baseline(&work.name, outcome));
    }
    for line in lines(base) {
        let mut row = Row::new(line.kind, &line.label());
        row.needed = Mark::No;
        for (work, was) in workloads.iter().zip(&whole) {
            let text =
                profile_of(&work.argv).ok_or_else(|| TraceError::NoProfile(work.name.clone()))?;
            let argv = with_profile(&work.argv, &without(&text, base, &line)?)
                .ok_or_else(|| TraceError::NoProfile(work.name.clone()))?;
            let now = run(work, &argv);
            if work.allowed && *was == Outcome::Success && now != Outcome::Success {
                row.needed = Mark::Yes;
                row.workloads.push(work.name.clone());
            }
            if !work.allowed && now != *was {
                row.denial_changed.push(work.name.clone());
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One path an `fs_usage` line names, and the process that used it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    /// The path as the process spelled it.
    pub path: String,
    /// The process name, without the thread id.
    pub process: String,
}

/// The paths of `fs_usage -w -f pathname` output: for each line the first path and the
/// last column, the process (`name.tid`). Lines with no path are skipped.
#[must_use]
pub fn parse_fs_usage(text: &str) -> Vec<Touch> {
    let mut touches = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let (Some(last), Some(first)) = (
            words.last(),
            words.iter().position(|word| word.starts_with('/')),
        ) else {
            continue;
        };
        let mut end = words.len() - 1;
        if end > first && words[end - 1] == "W" {
            end -= 1;
        }
        if end > first && words[end - 1].parse::<f64>().is_ok() {
            end -= 1;
        }
        if end <= first {
            continue;
        }
        let process = match last.rsplit_once('.') {
            Some((name, tid)) if tid.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => last,
        };
        touches.push(Touch {
            path: words[first..end].join(" "),
            process: process.to_owned(),
        });
    }
    touches
}

/// The path the tracer stats (and that does not exist) before a workload.
#[must_use]
pub fn begin_marker(name: &str) -> String {
    format!("/js-trace-begin-{name}")
}

/// The path the tracer stats after a workload.
#[must_use]
pub fn end_marker(name: &str) -> String {
    format!("/js-trace-end-{name}")
}

/// The touches between a workload's begin and end markers, by workload. A window whose
/// end was not captured, or that another begin interrupted, is dropped; one workload run
/// again adds to its window.
#[must_use]
pub fn windows(touches: &[Touch]) -> BTreeMap<String, Vec<Touch>> {
    let mut done: BTreeMap<String, Vec<Touch>> = BTreeMap::new();
    let mut open: Option<(String, Vec<Touch>)> = None;
    for touch in touches {
        if let Some(name) = touch.path.strip_prefix("/js-trace-begin-") {
            open = Some((name.to_owned(), Vec::new()));
        } else if let Some(name) = touch.path.strip_prefix("/js-trace-end-") {
            if let Some((current, inside)) = open.take() {
                if current == name {
                    done.entry(current).or_default().extend(inside);
                }
            }
        } else if let Some((_, inside)) = open.as_mut() {
            inside.push(touch.clone());
        }
    }
    done
}

fn private(path: &str) -> String {
    for link in ["/var", "/etc", "/tmp"] {
        if path == link || path.starts_with(&format!("{link}/")) {
            return format!("/private{path}");
        }
    }
    path.to_owned()
}

fn below(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a process that used `path` used `line`: a name when the path is spelled
/// through it; the root literal for any path (every lookup starts there); a literal for
/// that file; a subpath for the directory or anything below it. `/var/x` and
/// `/private/var/x` are the same place.
#[must_use]
pub fn covers(line: &Line, path: &str) -> bool {
    let real = private(path);
    match (line.kind, line.how) {
        (Kind::Name, _) => below(path, &line.path),
        (_, Match::Literal) => line.path == "/" || path == line.path || real == line.path,
        (_, Match::Subpath) => below(path, &line.path) || below(&real, &line.path),
    }
}

/// Fills `touched` and `touched_by` of the name and read rows from `windows`. When a
/// workload of `expected` has no window, a line nothing touched is `?`, not `no`.
pub fn mark_touched(
    rows: &mut [Row],
    base: &BaseSet,
    windows: &BTreeMap<String, Vec<Touch>>,
    expected: &[String],
) {
    let complete = expected.iter().all(|name| windows.contains_key(name));
    let by_label: BTreeMap<(&str, String), Line> = lines(base)
        .into_iter()
        .map(|line| ((line.kind.as_str(), line.label()), line))
        .collect();
    for row in rows.iter_mut() {
        let Some(line) = by_label.get(&(row.kind.as_str(), row.line.clone())) else {
            continue;
        };
        let mut by = BTreeSet::new();
        for (name, inside) in windows {
            for touch in inside.iter().filter(|touch| covers(line, &touch.path)) {
                by.insert(format!("{name}/{}", touch.process));
            }
        }
        row.touched = match (by.is_empty(), complete) {
            (false, _) => Mark::Yes,
            (true, true) => Mark::No,
            (true, false) => Mark::Unknown,
        };
        row.touched_by = by.into_iter().collect();
    }
}

/// One row per setuid or setgid file found: [`Kind::Setuid`] when `listed` has it,
/// [`Kind::SetuidNew`] when not.
#[must_use]
pub fn setuid_rows(found: &[String], listed: &[&str]) -> Vec<Row> {
    found
        .iter()
        .map(|path| {
            let kind = if listed.contains(&path.as_str()) {
                Kind::Setuid
            } else {
                Kind::SetuidNew
            };
            Row::new(kind, path)
        })
        .collect()
}
