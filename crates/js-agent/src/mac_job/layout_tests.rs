//! The job folder's layout and the environment a job gets.

use std::path::{Path, PathBuf};

use js_types::Arch;

use super::{Layout, PATH, SHELL, SUBFOLDERS};
use crate::mac_job::spec::JobFile;

fn job(env: &str) -> JobFile {
    let text =
        format!(r#"{{"id":"j","os":"macos","program":"/bin/sh","wall_limit_secs":1,"env":{env}}}"#);
    JobFile::parse(&text, Arch::Arm64).expect("decodes")
}

#[test]
fn the_folders_are_under_the_root_by_the_job_id() {
    let layout = Layout::new(Path::new("/private/var/js"), "j-1");
    assert_eq!(layout.folder, PathBuf::from("/private/var/js/j-1"));
    assert_eq!(layout.work(), PathBuf::from("/private/var/js/j-1/work"));
    assert_eq!(layout.home(), PathBuf::from("/private/var/js/j-1/home"));
    assert_eq!(layout.tmp(), PathBuf::from("/private/var/js/j-1/tmp"));
    assert_eq!(layout.cache(), PathBuf::from("/private/var/js/j-1/cache"));
    assert_eq!(SUBFOLDERS, ["work", "home", "tmp", "cache"]);
}

#[test]
fn home_temp_and_caches_point_inside_the_job_folder_and_the_jobs_own_variables_stay() {
    let layout = Layout::new(Path::new("/r"), "j");
    let env = layout.env(&job(r#"{"LANG":"C"}"#), "kbf-lease-pool-80");
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        pairs,
        [
            ("CFFIXED_USER_HOME", "/r/j/home"),
            ("CLANG_MODULE_CACHE_PATH", "/r/j/cache/clang"),
            ("HOME", "/r/j/home"),
            ("LANG", "C"),
            ("LOGNAME", "kbf-lease-pool-80"),
            ("PATH", PATH),
            ("SHELL", SHELL),
            ("TMPDIR", "/r/j/tmp/"),
            ("USER", "kbf-lease-pool-80"),
            ("XDG_CACHE_HOME", "/r/j/cache"),
        ]
    );
}
