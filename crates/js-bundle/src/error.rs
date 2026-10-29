//! The errors of this crate. Each says what was wrong and, for a config, where.

use std::path::PathBuf;

use crate::names::TreeName;
use crate::template::is_forbidden_char;

/// `message` with its control characters escaped and its length cut to
/// [`MAX_JSON_ERROR_BYTES`] (the cut is marked with `...`). A parser echoes keys and values
/// of the document, which a writer controls: unescaped they would split a log line, and
/// unbounded they would bloat it.
pub(crate) fn bounded_json_message(message: &str) -> String {
    const MARK: &str = "...";
    let mut out = String::new();
    for c in message.chars() {
        if is_forbidden_char(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
        if out.len() > MAX_JSON_ERROR_BYTES {
            while out.len() > MAX_JSON_ERROR_BYTES - MARK.len() {
                out.pop();
            }
            out.push_str(MARK);
            break;
        }
    }
    out
}

/// Why a placeholder string is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The text is over the length limit.
    #[error("value is longer than {max} bytes")]
    TooLong {
        /// The limit.
        max: usize,
    },
    /// The text holds a NUL byte.
    #[error("value holds a NUL byte")]
    Nul,
    /// The text holds a line break.
    #[error("value holds a newline")]
    Newline,
    /// The text holds another control character: a tab, an escape, a bidirectional
    /// control, a line or paragraph separator.
    #[error("value holds the control character U+{code:04X}")]
    Control {
        /// The character's code point.
        code: u32,
    },
    /// A `${` with no closing brace.
    #[error("placeholder is not closed")]
    Unterminated,
    /// A placeholder other than `${tree:NAME}` and `${developer_dir}`.
    #[error("unknown placeholder ${{{0}}}")]
    Unknown(String),
    /// A `${tree:NAME}` whose name is not a tree name.
    #[error("placeholder names no valid tree: {0:?}")]
    BadTreeName(String),
    /// Text glued to a placeholder other than through a `/`, or two placeholders
    /// side by side: the result would not be a path below the placeholder.
    #[error("text is attached to a placeholder without a '/'")]
    Attached,
    /// A path with an empty component (`//` or a trailing `/`).
    #[error("path has an empty component")]
    EmptyComponent,
    /// A path with a `.` component.
    #[error("path has a '.' component")]
    CurrentDir,
    /// A path with a `..` component.
    #[error("path has a '..' component")]
    ParentDir,
}

/// Why an environment name is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvNameError {
    /// Not `[A-Za-z_][A-Za-z0-9_]*`.
    #[error("not a valid environment name")]
    Malformed,
    /// A name the agent owns or the loader or sandbox reads.
    #[error("the name is reserved")]
    Forbidden,
    /// Longer than [`js_types::env::MAX_ENV_NAME_LEN`] bytes.
    #[error("the name is longer than {max} bytes")]
    TooLong {
        /// The limit.
        max: usize,
    },
}

/// The longest message of [`ConfigError::Json`], in bytes: what the parser echoes of the
/// document (a key, a value) is escaped and cut to this.
pub const MAX_JSON_ERROR_BYTES: usize = 512;

/// What is wrong with one field of a config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Defect {
    /// Absent or empty where a value is required.
    #[error("must not be empty")]
    Empty,
    /// Longer than the limit.
    #[error("longer than {max}")]
    TooLong {
        /// The limit.
        max: usize,
    },
    /// More entries than the limit.
    #[error("more than {max} entries")]
    TooMany {
        /// The limit.
        max: usize,
    },
    /// Outside the grammar of the field.
    #[error("must be {want}")]
    Malformed {
        /// The grammar, in words.
        want: &'static str,
    },
    /// Not the one value this format version allows.
    #[error("must be {want}")]
    Unsupported {
        /// The only allowed value.
        want: &'static str,
    },
    /// Appears twice.
    #[error("appears twice")]
    Duplicate,
    /// Over a ceiling the format sets on a number.
    #[error("over the limit of {max}")]
    OverLimit {
        /// The limit.
        max: u64,
    },
    /// Zero where at least one is required.
    #[error("must be at least 1")]
    Zero,
    /// Names a tree the config does not declare.
    #[error("names the undeclared tree {0}")]
    Undeclared(String),
    /// The wrong number of values for the operation.
    #[error("wrong number of values for the operation")]
    Arity,
    /// A bad environment name.
    #[error("{0}")]
    EnvName(#[from] EnvNameError),
}

/// A config that is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The document is over the size limit; it was not read.
    #[error("config is {len} bytes, over the limit of {max}")]
    TooLarge {
        /// The document's length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The document is not JSON of this schema (including an unknown field).
    #[error("config does not parse: {0}")]
    Json(String),
    /// A field that is refused.
    #[error("{field}: {defect}")]
    Invalid {
        /// The field, in the config's own spelling (`layers[1].license`).
        field: String,
        /// What is wrong with it.
        defect: Defect,
    },
}

/// Why a placeholder string cannot be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The text is not a valid placeholder string.
    #[error("{0}")]
    Template(#[from] TemplateError),
    /// A `${tree:NAME}` for a tree the bundle does not have.
    #[error("the bundle has no tree {0:?}")]
    UnknownTree(TreeName),
    /// A `${developer_dir}` with no Xcode selected.
    #[error("no Xcode is selected for ${{developer_dir}}")]
    NoDeveloperDir,
}

/// Why a bundle cannot be materialised for a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaterialiseError {
    /// The config is not valid.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// A path the caller gave is not an absolute, normalised UTF-8 path without
    /// control characters or `:`.
    #[error("not an absolute normalised path: {0:?}")]
    BadPath(PathBuf),
    /// The config lists a writable tree and the caller gave no clone for it.
    #[error("no writable clone for tree {0:?}")]
    MissingWritable(TreeName),
    /// The caller gave a writable clone for a tree the config does not list.
    #[error("tree {0:?} is not a writable tree of the bundle")]
    UnexpectedWritable(TreeName),
    /// A placeholder string of the run cannot be expanded.
    #[error("{what}: {source}")]
    Expand {
        /// Which string: `env NAME`, `arg N` or `entrypoint NAME`.
        what: String,
        /// Why.
        source: ExpandError,
    },
    /// The job sets an environment name it may not.
    #[error("job env {name:?}: {why}")]
    JobEnvName {
        /// The name.
        name: String,
        /// Why.
        why: EnvNameError,
    },
    /// The environment, as it would be handed to the program, is over the limit.
    #[error("the environment is {len} bytes, over the limit of {max}")]
    EnvTooLarge {
        /// The size: every name and value, with the `=` and the terminator of each.
        len: usize,
        /// The limit, [`crate::MAX_ENV_BYTES`].
        max: usize,
    },
    /// The job sets a name the bundle locked.
    #[error("job env {0:?} is locked by the bundle")]
    Locked(String),
}
