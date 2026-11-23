//! Who the job runs as, the places a probe tries to write and read, and the arguments that
//! name host-specific ones (`NAME=VALUE`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The account a job runs as, as the native runtime sets it up: the uid, and `USER` and `HOME`,
/// which the helper sets for every run and a job cannot replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// The user id.
    pub uid: u32,
    /// The user's name.
    pub user: String,
    /// The user's home: the run directory, all of which the profile lets the job write.
    pub home: PathBuf,
    /// The job's working directory (`<home>/exec`).
    pub cwd: PathBuf,
    /// The user's `DARWIN_USER_TEMP_DIR` under `/private/var/folders`, when the system names
    /// one.
    pub user_temp: Option<PathBuf>,
}

impl Who {
    /// The account of this process: `uid`, `USER` and `HOME` from `env`, and `cwd`.
    ///
    /// # Errors
    /// The name of a variable that is not set or not UTF-8, or uid 0: the probe runs as a
    /// job's user, never as root, whose writes would succeed where a job's must not.
    pub fn from_env(
        uid: u32,
        env: impl Fn(&str) -> Option<OsString>,
        cwd: PathBuf,
        user_temp: Option<PathBuf>,
    ) -> Result<Self, String> {
        if uid == 0 {
            return Err("it runs as a job's user, never as root".to_owned());
        }
        let user = env("USER")
            .and_then(|v| v.into_string().ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "USER is not set".to_owned())?;
        let home = env("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_owned())?;
        Ok(Self {
            uid,
            user,
            home,
            cwd,
            user_temp,
        })
    }

    /// The same places for the account that left markers: `uid`, `user` and `home` from the
    /// leaver, the working directory at the same place under the leaver's home, and no user
    /// temp directory (another account's is not known).
    #[must_use]
    pub fn leaver(&self, uid: u32, user: &str, home: &Path) -> Self {
        let cwd = self
            .cwd
            .strip_prefix(&self.home)
            .map_or_else(|_| self.cwd.clone(), |rest| home.join(rest));
        Self {
            uid,
            user: user.to_owned(),
            home: home.to_path_buf(),
            cwd,
            user_temp: None,
        }
    }
}

/// A place a probe writes a file in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The probe's name after `write:`.
    pub name: String,
    /// The directory. The probe never makes it: when it is absent the probe is `n/a`.
    pub dir: PathBuf,
}

impl Target {
    fn new(name: &str, dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.to_owned(),
            dir: dir.into(),
        }
    }
}

/// The places every write probe tries, for the account `who`: the run directory, which must
/// be writable, and the places a job must not write: the shared temporary and cache folders,
/// the package managers' prefixes, `/etc`, the system's launch daemons, and the user's own
/// launch agents and preferences. Those two are tried only when the home has them: a folder
/// a job makes under `~/Library` is one a job's sandbox cannot remove again, and so neither can
/// the probe.
#[must_use]
pub fn writes(who: &Who) -> Vec<Target> {
    let mut targets = vec![
        Target::new("run-dir", who.cwd.clone()),
        Target::new("private-tmp", "/private/tmp"),
        Target::new("private-var-tmp", "/private/var/tmp"),
        Target::new("users-shared", "/Users/Shared"),
        Target::new("library-caches", "/Library/Caches"),
        Target::new("opt-homebrew", "/opt/homebrew"),
        Target::new("usr-local", "/usr/local"),
        Target::new("etc", "/private/etc"),
        Target::new("library-launchdaemons", "/Library/LaunchDaemons"),
        Target::new("home-launchagents", who.home.join("Library/LaunchAgents")),
        Target::new("home-preferences", who.home.join("Library/Preferences")),
    ];
    if let Some(dir) = &who.user_temp {
        targets.push(Target::new("user-temp-dir", dir.clone()));
    }
    targets
}

/// The reads every read probe tries: the host's sudoers file.
#[must_use]
pub fn reads() -> Vec<Named> {
    vec![Named {
        name: "sudoers".to_owned(),
        value: "/private/etc/sudoers".to_owned(),
    }]
}

/// A host-specific place or address an argument names: `NAME=VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// Lower-case letters, digits and `-`.
    pub name: String,
    /// The path, `HOST:PORT` or host name.
    pub value: String,
}

/// `NAME=VALUE`, for clap.
///
/// # Errors
/// What is wrong with the argument.
pub fn named(arg: &str) -> Result<Named, String> {
    let (name, value) = arg
        .split_once('=')
        .ok_or_else(|| format!("{arg:?} is not NAME=VALUE"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "{name:?} is not a name: lower-case letters, digits and '-'"
        ));
    }
    if value.is_empty() {
        return Err(format!("{name}: the value is empty"));
    }
    Ok(Named {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// The targets `--write NAME=DIR` names, after the built-in ones.
#[must_use]
pub fn with_extra(mut targets: Vec<Target>, extra: &[Named]) -> Vec<Target> {
    targets.extend(extra.iter().map(|n| Target::new(&n.name, &n.value)));
    targets
}

#[cfg(test)]
pub(crate) fn test_who(home: &Path) -> Who {
    Who {
        uid: 601,
        user: "pool-1".to_owned(),
        home: home.to_path_buf(),
        cwd: home.join("exec"),
        user_temp: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn who_comes_from_user_and_home() {
        let who = Who::from_env(
            600,
            env(&[("USER", "pool-0"), ("HOME", "/Users/pool-0")]),
            PathBuf::from("/Users/pool-0/exec"),
            Some(PathBuf::from("/private/var/folders/x/T")),
        )
        .unwrap();
        assert_eq!(who.user, "pool-0");
        assert_eq!(who.home, Path::new("/Users/pool-0"));
        assert_eq!(who.uid, 600);
    }

    #[test]
    fn an_unset_or_empty_user_or_home_is_refused_by_name() {
        let cwd = PathBuf::from("/");
        assert_eq!(
            Who::from_env(
                0,
                env(&[("USER", "root"), ("HOME", "/var/root")]),
                cwd.clone(),
                None
            ),
            Err("it runs as a job's user, never as root".to_owned())
        );
        assert_eq!(
            Who::from_env(1, env(&[("HOME", "/h")]), cwd.clone(), None),
            Err("USER is not set".to_owned())
        );
        assert_eq!(
            Who::from_env(1, env(&[("USER", ""), ("HOME", "/h")]), cwd.clone(), None),
            Err("USER is not set".to_owned())
        );
        assert_eq!(
            Who::from_env(1, env(&[("USER", "u"), ("HOME", "")]), cwd, None),
            Err("HOME is not set".to_owned())
        );
    }

    #[test]
    fn the_leaver_has_the_same_places_under_its_own_home() {
        let me = test_who(Path::new("/Users/pool-1"));
        let them = me.leaver(600, "pool-0", Path::new("/Users/pool-0"));
        assert_eq!(them.cwd, Path::new("/Users/pool-0/exec"));
        assert_eq!(them.user, "pool-0");
        assert_eq!(them.uid, 600);
        let odd = Who {
            cwd: PathBuf::from("/elsewhere"),
            ..me
        };
        assert_eq!(
            odd.leaver(600, "pool-0", Path::new("/Users/pool-0")).cwd,
            Path::new("/elsewhere")
        );
    }

    #[test]
    fn the_writes_are_the_run_dir_the_system_places_and_the_users_own() {
        let mut who = test_who(Path::new("/Users/pool-1"));
        let names: Vec<String> = writes(&who).into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "run-dir",
                "private-tmp",
                "private-var-tmp",
                "users-shared",
                "library-caches",
                "opt-homebrew",
                "usr-local",
                "etc",
                "library-launchdaemons",
                "home-launchagents",
                "home-preferences"
            ]
        );
        who.user_temp = Some(PathBuf::from("/private/var/folders/x/T"));
        let last = writes(&who).pop().unwrap();
        assert_eq!(last.name, "user-temp-dir");
        assert_eq!(reads()[0].value, "/private/etc/sudoers");
    }

    #[test]
    fn a_named_argument_is_name_equals_value() {
        assert_eq!(
            named("job-service=192.0.2.1:7302"),
            Ok(Named {
                name: "job-service".to_owned(),
                value: "192.0.2.1:7302".to_owned()
            })
        );
        assert_eq!(
            named("novalue"),
            Err("\"novalue\" is not NAME=VALUE".to_owned())
        );
        assert!(named("Bad=x").unwrap_err().contains("is not a name"));
        assert!(named("=x").unwrap_err().contains("is not a name"));
        assert_eq!(named("a="), Err("a: the value is empty".to_owned()));
        assert_eq!(named("lan-host2=192.0.2.7:22").unwrap().name, "lan-host2");
        let all = with_extra(vec![], &[named("bundle-store=/s").unwrap()]);
        assert_eq!(all[0].dir, Path::new("/s"));
    }
}
