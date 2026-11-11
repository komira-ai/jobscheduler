//! The workflow lint of Debian packages, over every workflow in `.github/workflows`, read as
//! text.
//!
//! No `run:` line names a Canonical host (`ubuntu.com`, `launchpad.net`), and none runs
//! `apt-get`, `apt` or `aptitude` (after an optional `sudo`) but in the workflows of
//! [`APT_EXEMPT`]: a CI job never needs the Ubuntu archive. A package a job needs and the runner image lacks is
//! pinned in `.github/debs/<set>.toml` and installed by four steps in a row, all under one `if:`
//! or none:
//!   - `actions/cache/restore` pinned by commit, with an `id:`, and in its `with:` exactly the
//!     path `${{ runner.temp }}/debs/<set>` and the key
//!     `debs-<set>-${{ runner.arch }}-${{ hashFiles('.github/debs/<set>.toml') }}`: a new pin is
//!     a new key, and no `restore-keys` can hand back another pin's files;
//!   - `cargo xtask debs --pins .github/debs/<set>.toml --dir "$RUNNER_TEMP/debs/<set>"`, one
//!     command, which checks every file and downloads only the missing ones;
//!   - `actions/cache/save` pinned by commit, run only on a miss (`if:` the steps' condition,
//!     if any, `&& steps.<id>.outputs.cache-hit != 'true'`) and only once the fetch passed, with
//!     the same path and `${{ steps.<id>.outputs.cache-primary-key }}`;
//!   - `sudo dpkg --install "$RUNNER_TEMP"/debs/<set>/*.deb`, the only `dpkg --install` a
//!     workflow may run.
//!
//! `cargo xtask debs` refuses a directory holding a file no pin names, so that glob is the
//! pinned packages.

use super::lint::{Step, jobs, pinned, words};

/// The workflows that may run apt: `machine-image.yml`, run by hand only, installs QEMU from
/// pinned and checked .debs and lets apt take their libraries from the archive.
pub(crate) const APT_EXEMPT: [&str; 1] = ["machine-image.yml"];

/// One set of pinned packages a job installs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DebsSite {
    pub(crate) job: String,
    pub(crate) set: String,
}

/// A step's `if:`, without `${{ }}` around it.
fn condition(step: &Step) -> Option<&str> {
    step.get("if").map(|c| {
        c.strip_prefix("${{")
            .and_then(|c| c.strip_suffix("}}"))
            .unwrap_or(c)
            .trim()
    })
}

/// The words of `line` without a leading `sudo`.
fn command(line: &str) -> Vec<String> {
    let mut words = words(line.trim());
    if words.first().is_some_and(|w| w == "sudo") {
        words.remove(0);
    }
    words
}

/// The package sets the workflow `file` with text `text` installs, or the reasons it breaks
/// the rule, each as `<file>:<line>: <job>: <why>`.
pub(crate) fn lint_debs(file: &str, text: &str) -> Result<Vec<DebsSite>, Vec<String>> {
    let mut sites = Vec::new();
    let mut errors = Vec::new();
    for job in jobs(text) {
        let mut installs = Vec::new();
        for (i, step) in job.steps.iter().enumerate() {
            let Some(run) = step.get("run") else {
                continue;
            };
            if run.contains("xtask debs") {
                // The install two steps on is this site's, kept or broken: a broken site is
                // reported once, here.
                installs.push(i + 2);
                match debs_site(&job.steps, i) {
                    Ok(set) => {
                        sites.push(DebsSite {
                            job: job.name.clone(),
                            set,
                        });
                    }
                    Err(why) => errors.push(format!("{file}:{}: {}: {why}", step.line, job.name)),
                }
            }
        }
        for (i, step) in job.steps.iter().enumerate() {
            let mut fail =
                |why: String| errors.push(format!("{file}:{}: {}: {why}", step.line, job.name));
            for line in step.get("run").unwrap_or_default().lines() {
                let words = command(line);
                let first = words.first().map_or("", String::as_str);
                if ["apt-get", "apt", "aptitude"].contains(&first) && !APT_EXEMPT.contains(&file) {
                    fail(format!(
                        "runs {first}: pin the packages in .github/debs and install them with \
                         `cargo xtask debs`"
                    ));
                }
                if ["ubuntu.com", "launchpad.net"]
                    .iter()
                    .any(|h| line.contains(h))
                {
                    fail("names a Canonical host outside .github/debs".to_owned());
                }
                let installs_deb = words.iter().any(|w| w == "--install" || w == "-i");
                if first == "dpkg" && installs_deb && !installs.contains(&i) {
                    fail("a dpkg install that is not the last of the four debs steps".to_owned());
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(sites)
    } else {
        Err(errors)
    }
}

/// The set of the `xtask debs` at `steps[i]`, once it and the steps around it keep the rule.
fn debs_site(steps: &[Step], i: usize) -> Result<String, String> {
    let debs = &steps[i];
    let run = debs.get("run").unwrap_or_default();
    for chain in ["\n", "&&", "||", ";", "|", "`", "$(", ">", "${{"] {
        if run.contains(chain) {
            return Err(format!(
                "the debs step is not one command ({chain:?} in it)"
            ));
        }
    }
    let words = words(run);
    let set = match words.as_slice() {
        [cargo, xtask, verb, pins_flag, pins, dir_flag, dir]
            if [cargo, xtask, verb, pins_flag, dir_flag]
                == [&"cargo", &"xtask", &"debs", &"--pins", &"--dir"] =>
        {
            let set = pins
                .strip_prefix(".github/debs/")
                .and_then(|p| p.strip_suffix(".toml"))
                .filter(|s| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
                .ok_or_else(|| format!("the --pins {pins:?} is not .github/debs/<set>.toml"))?;
            if *dir != format!("$RUNNER_TEMP/debs/{set}") {
                return Err(format!(
                    "the --dir {dir:?} is not \"$RUNNER_TEMP/debs/{set}\""
                ));
            }
            set.to_owned()
        }
        _ => {
            return Err(
                "the debs step is not `cargo xtask debs --pins <file> --dir <dir>`".to_owned(),
            );
        }
    };
    let cond = condition(debs);
    let path = format!("${{{{ runner.temp }}}}/debs/{set}");

    let restore = i
        .checked_sub(1)
        .map(|r| &steps[r])
        .ok_or("no actions/cache/restore step right before the debs step")?;
    if !pinned(restore.uses(), "actions/cache/restore") {
        return Err(format!(
            "the step before the debs step (line {}) is not actions/cache/restore pinned by commit",
            restore.line
        ));
    }
    if condition(restore) != cond {
        return Err(format!(
            "the restore's `if:` is {:?}, not the debs step's {cond:?}",
            condition(restore)
        ));
    }
    let id = restore.get("id").ok_or("the restore has no `id:`")?;
    let key = format!(
        "debs-{set}-${{{{ runner.arch }}}}-${{{{ hashFiles('.github/debs/{set}.toml') }}}}"
    );
    expect_with(restore, "restore", &path, &key)?;

    let save = steps
        .get(i + 1)
        .ok_or("no actions/cache/save step right after the debs step")?;
    if !pinned(save.uses(), "actions/cache/save") {
        return Err(format!(
            "the step after the debs step (line {}) is not actions/cache/save pinned by commit",
            save.line
        ));
    }
    let hit = format!("steps.{id}.outputs.cache-hit != 'true'");
    let want = cond.map_or(hit.clone(), |c| format!("{c} && {hit}"));
    if condition(save) != Some(want.as_str()) {
        return Err(format!(
            "the save's `if:` is {:?}, not {want:?}",
            condition(save)
        ));
    }
    let primary = format!("${{{{ steps.{id}.outputs.cache-primary-key }}}}");
    expect_with(save, "save", &path, &primary)?;

    let install = steps
        .get(i + 2)
        .ok_or("no dpkg install step right after the save")?;
    let want = format!("sudo dpkg --install \"$RUNNER_TEMP\"/debs/{set}/*.deb");
    if install.get("run") != Some(want.as_str()) || condition(install) != cond {
        return Err(format!(
            "the step after the save (line {}) is not `{want}` under the debs step's `if:`",
            install.line
        ));
    }
    Ok(set)
}

/// `step`'s `with:` is exactly `path` and `key`.
fn expect_with(step: &Step, what: &str, path: &str, key: &str) -> Result<(), String> {
    let keys: Vec<&str> = step.with.keys().map(String::as_str).collect();
    if keys != ["key", "path"] {
        return Err(format!(
            "the {what}'s `with:` has {keys:?}, not exactly path and key"
        ));
    }
    if step.with["path"] != path {
        return Err(format!(
            "the {what}'s path is {:?}, not {path:?}",
            step.with["path"]
        ));
    }
    if step.with["key"] != key {
        return Err(format!(
            "the {what}'s key is {:?}, not {key:?}",
            step.with["key"]
        ));
    }
    Ok(())
}
