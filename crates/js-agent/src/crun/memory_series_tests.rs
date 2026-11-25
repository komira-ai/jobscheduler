use js_testkit::dir::TestDir;

use super::*;

/// Asserts the non-file memory of a group: `memory.current` less `file - shmem`, so shared
/// memory and tmpfs stay counted and page cache does not; never below 0; none without `file`
/// or `shmem`. Catches page cache counted as the job's memory, and tmpfs pages dropped with it.
#[test]
fn non_file_memory_is_current_less_file_that_is_not_shmem() {
    let stat = |file: u64, shmem: u64| format!("anon 1\nfile {file}\nkernel 3\nshmem {shmem}\n");
    assert_eq!(nonfile(10_000, &stat(6_000, 1_000)), Some(5_000));
    assert_eq!(nonfile(10_000, &stat(6_000, 6_000)), Some(10_000));
    assert_eq!(nonfile(1_000, &stat(6_000, 0)), Some(0));
    assert_eq!(nonfile(1_000, &stat(0, 9)), Some(1_000));
    assert_eq!(nonfile(1_000, "anon 1\nshmem 2\n"), None);
    assert_eq!(nonfile(1_000, "anon 1\nfile 2\n"), None);
}

/// Asserts that a group's files are read from its directory, and that a missing or unreadable
/// one gives none.
#[test]
fn non_file_memory_is_read_from_the_groups_files() {
    let dir = TestDir::under(std::env::temp_dir(), "memory-series-nonfile");
    let at = dir.path();
    assert_eq!(nonfile_of(at), None);
    std::fs::write(at.join("memory.current"), "4096\n").unwrap();
    assert_eq!(nonfile_of(at), None);
    std::fs::write(at.join("memory.stat"), "file 1024\nshmem 24\n").unwrap();
    assert_eq!(nonfile_of(at), Some(3_096));
    std::fs::write(at.join("memory.current"), "x\n").unwrap();
    assert_eq!(nonfile_of(at), None);
}
