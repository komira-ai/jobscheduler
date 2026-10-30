//! The reserved environment names: every prefix and owned name is pinned, so a change
//! to the list is a visible change to this table.

use js_types::env::{MAX_ENV_NAME_LEN, OWNED_ENV_NAMES, RESERVED_ENV_PREFIXES, SHELL_ENV_NAMES, env_name_reserved};

/// The list is exactly the union the loader, the sandbox, launchd, the allocator, the
/// Objective-C runtime and the agent need.
#[test]
fn the_reserved_lists_are_pinned() {
    assert_eq!(
        RESERVED_ENV_PREFIXES,
        [
            "DYLD_",
            "LD_",
            "SANDBOX_",
            "JS_",
            "__XPC_",
            "MALLOC",
            "Malloc",
            "OBJC_",
            "BASH_FUNC_"
        ]
    );
    assert_eq!(
        SHELL_ENV_NAMES,
        [
            "SHELLOPTS",
            "BASHOPTS",
            "BASH_ENV",
            "ENV",
            "PS4",
            "PROMPT_COMMAND",
            "IFS",
            "CDPATH"
        ]
    );
    assert_eq!(
        OWNED_ENV_NAMES,
        [
            "HOME",
            "TMPDIR",
            "CFFIXED_USER_HOME",
            "XDG_CACHE_HOME",
            "CLANG_MODULE_CACHE_PATH",
            "DEVELOPER_DIR",
        ]
    );
    assert_eq!(MAX_ENV_NAME_LEN, 128);
}

/// Names that start with a reserved prefix, and the owned names, are reserved; near
/// misses are not (the match is a case-sensitive prefix, or the whole name).
#[test]
fn reserved_names_are_refused_and_near_misses_are_not() {
    let reserved = [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_",
        "LD_PRELOAD",
        "SANDBOX_X",
        "JS_AGENT_RUN",
        "JS_FOO",
        "JS_",
        "__XPC_X",
        "__XPC_DYLD_LIBRARY_PATH",
        "MALLOC_CONF",
        "MALLOCX",
        "MallocStackLogging",
        "MallocLogFile",
        "OBJC_DEBUG",
        "OBJC_PRINT_IMAGES",
        "HOME",
        "TMPDIR",
        "CFFIXED_USER_HOME",
        "XDG_CACHE_HOME",
        "CLANG_MODULE_CACHE_PATH",
        "DEVELOPER_DIR",
        "SHELLOPTS",
        "BASHOPTS",
        "BASH_ENV",
        "ENV",
        "PS4",
        "PROMPT_COMMAND",
        "IFS",
        "CDPATH",
        "BASH_FUNC_ulimit%%",
        "BASH_FUNC_",
    ];
    for name in reserved {
        assert!(env_name_reserved(name), "{name}");
    }
    let free = [
        "PATH",
        "ZIG_LIB_DIR",
        "LDX",
        "LIBRARY_PATH",
        "dyld_lower",
        "JS",
        "JSX",
        "__X",
        "MALLO",
        "malloc_conf",
        "OBJC",
        "HOMEX",
        "XHOME",
        "home",
        "TMPDIRS",
        "DEVELOPER_DIRS",
        "ENVIRONMENT",
        "PS1",
        "PS44",
        "BASH",
        "BASH_FUNC",
        "shellopts",
        "",
    ];
    for name in free {
        assert!(!env_name_reserved(name), "{name}");
    }
}

/// Every listed prefix refuses a name made from it, and every owned name is refused.
#[test]
fn every_listed_entry_is_reserved() {
    for prefix in RESERVED_ENV_PREFIXES {
        assert!(env_name_reserved(prefix), "{prefix}");
        assert!(env_name_reserved(&format!("{prefix}X")), "{prefix}X");
    }
    for name in OWNED_ENV_NAMES {
        assert!(env_name_reserved(name), "{name}");
    }
}

/// Every shell-affecting name is refused: a shell reads each before its first command, so
/// a job that could set one would steer the shell that starts it.
#[test]
fn every_shell_name_is_reserved() {
    for name in SHELL_ENV_NAMES {
        assert!(env_name_reserved(name), "{name}");
    }
}
