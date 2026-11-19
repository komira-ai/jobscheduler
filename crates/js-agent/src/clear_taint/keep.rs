//! What a clear keeps, and the record of it: the names the files are kept under, the moves
//! that never write over a file, and the record in the state root.
//!
//! | file | kept as |
//! |---|---|
//! | `taint.json` | `taint.json.cleared-<stamp>-by-<operator>` |
//! | `taint.pinned` | `taint.pinned.cleared-<stamp>-by-<operator>` |
//! | `owed-<run>` | `owed.<run>.cleared-<stamp>-by-<operator>` |
//! | (the record) | `taint.cleared-<stamp>-by-<operator>.json` |
//!
//! The stamp is the clear's time in Unix seconds, as the agent's own clear stamps the marker it
//! keeps (`taint.json.cleared-<stamp>`). A kept owed record does not begin `owed-`: the agent
//! reads every such name as a run whose checks are owed, and would taint the host again for it
//! at its next start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

use js_agent_wire::Taint;
use serde_json::json;

use crate::lifecycle::StateRoot;

/// The most bytes of an operator's name.
const MAX_OPERATOR: usize = 64;

/// Who clears the taint: 1 to [`MAX_OPERATOR`] of `A-Z a-z 0-9 . _ @ -`, not starting with `.`
/// or `-`, so it can stand in a file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Operator(String);

impl Operator {
    /// The operator `--by` names.
    ///
    /// # Errors
    /// The text names the flag and the rule.
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || b"._@-".contains(&b);
        let leads = !text.starts_with(['.', '-']);
        let fits = (1..=MAX_OPERATOR).contains(&text.len());
        match fits && leads && text.bytes().all(allowed) {
            true => Ok(Self(text.to_owned())),
            false => Err(format!(
                "--by: 1 to {MAX_OPERATOR} of A-Z a-z 0-9 . _ @ -, not starting with . or -"
            )),
        }
    }
}

/// When the clear was made: Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp(u64);

impl Stamp {
    /// The stamp of `now_ms`, milliseconds since the epoch.
    pub(super) fn at(now_ms: u64) -> Self {
        Self(now_ms / 1000)
    }
}

/// A file kept aside: its name, and the name it is kept under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) file: String,
    pub(super) kept_as: String,
}

/// The name `file` is kept under.
pub(super) fn kept_name(file: &str, stamp: Stamp, who: &Operator) -> String {
    let tail = format!("cleared-{}-by-{}", stamp.0, who.0);
    match file.strip_prefix("owed-") {
        Some(run) => format!("owed.{run}.{tail}"),
        None => format!("{file}.{tail}"),
    }
}

/// The name of the record of the clear.
pub(super) fn record_name(stamp: Stamp, who: &Operator) -> String {
    format!("taint.cleared-{}-by-{}.json", stamp.0, who.0)
}

/// Keeps each of `names` aside in `root`, in order, each under [`kept_name`], and makes the
/// moves durable. A move never writes over a file: a name that is taken stops the clear.
///
/// # Errors
/// A move or the sync of the directory failed: what was kept before it, and why.
pub(super) fn keep_all(
    root: &StateRoot,
    names: &[String],
    stamp: Stamp,
    who: &Operator,
) -> Result<Vec<Kept>, (Vec<Kept>, String)> {
    let mut kept = Vec::new();
    for file in names {
        let kept_as = kept_name(file, stamp, who);
        let from = root.path().join(file);
        let to = root.path().join(&kept_as);
        // A link to the new name fails when that name is taken, then the old name goes.
        let moved = fs::hard_link(&from, &to).and_then(|()| fs::remove_file(&from));
        if let Err(wrong) = moved {
            return Err((
                kept,
                format!("{file} could not be kept as {kept_as}: {wrong}"),
            ));
        }
        kept.push(Kept {
            file: file.clone(),
            kept_as,
        });
    }
    match sync(root) {
        Ok(()) => Ok(kept),
        Err(wrong) => Err((kept, format!("the state root could not be synced: {wrong}"))),
    }
}

/// Makes what was created or moved in the state root durable.
fn sync(root: &StateRoot) -> io::Result<()> {
    File::open(root.path()).and_then(|dir| dir.sync_all())
}

/// What was kept, in words: `kept a as b, c as d`, or that nothing was.
pub(super) fn said(kept: &[Kept]) -> String {
    match kept.is_empty() {
        true => "nothing was kept aside".to_owned(),
        false => {
            let each = kept
                .iter()
                .map(|one| format!("{} as {}", one.file, one.kept_as));
            format!("kept {}", each.collect::<Vec<_>>().join(", "))
        }
    }
}

/// The taint in words: its reason, and its check and run when it names them.
fn taint_said(taint: Option<&Taint>) -> String {
    match taint {
        None => "no marker".to_owned(),
        Some(taint) => {
            let check = taint.check.as_deref().unwrap_or("none");
            let run = taint.run_id.as_deref().unwrap_or("none");
            format!("reason {}, check {check}, run {run}", taint.reason)
        }
    }
}

/// The line that says what the clear did, for the journal and the last line.
pub(super) fn told(
    stamp: Stamp,
    who: &Operator,
    reason: &str,
    taint: Option<&Taint>,
    kept: &[Kept],
) -> String {
    format!(
        "cleared the taint ({}) at {} by {}: {}; reason: {reason}; the lease server keeps its \
         own record of the taint until the operator's ClearHostTaint",
        taint_said(taint),
        stamp.0,
        who.0,
        said(kept)
    )
}

/// Records the clear in `root` as [`record_name`]: written to a file of its own, synced, and
/// renamed into place, the directory synced. The record names when, by whom and why, the taint
/// the marker held (null when there was no marker), and each file and the name it is kept
/// under.
///
/// # Errors
/// A step failed; the text says which.
pub(super) fn record(
    root: &StateRoot,
    stamp: Stamp,
    who: &Operator,
    reason: &str,
    taint: Option<&Taint>,
    kept: &[Kept],
) -> Result<String, String> {
    let name = record_name(stamp, who);
    let kept: Vec<_> = kept
        .iter()
        .map(|one| json!({ "file": one.file, "kept_as": one.kept_as }))
        .collect();
    let body = json!({
        "cleared_at_unix_secs": stamp.0,
        "by": who.0,
        "reason": reason,
        "taint": taint,
        "kept": kept,
    });
    let temp = root.path().join(format!("{name}.tmp"));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| writeln!(file, "{body}").and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temp, root.path().join(&name)))
        .and_then(|()| sync(root))
        .map(|()| name.clone())
        .map_err(|wrong| format!("{name}: {wrong}"))
}
