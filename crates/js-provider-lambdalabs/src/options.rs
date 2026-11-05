//! The `[[option]]` rows of the configuration: the allow list of the Lambda instance types
//! the provider may use. Parsing and checking only; nothing chooses among the rows here.
//!
//! A row names an instance type and nothing else. The GPU facts of each type (its model,
//! count and memory) come from the crate's code (`catalog.rs`), never from configuration.
//! A row written for an earlier release may still carry `gpu_class` and `gpus`: they are
//! accepted, not checked and ignored, and each such row logs one warning that names its
//! instance type. Any other key in a row is refused.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::error::cut;

/// One instance type the provider may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    /// Lambda's name for the type, such as `gpu_1x_a10`.
    pub instance_type: String,
}

/// The rows, in the order the file lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionTable {
    rows: Vec<OptionRow>,
}

/// Why a table was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    /// The text is not TOML of the expected shape (an unknown key in a row, a missing
    /// key, a value of the wrong type).
    #[error("option table: {0}")]
    Syntax(String),
    /// The text holds no `[[option]]` row.
    #[error("option table: no [[option]] row")]
    NoRows,
    /// A row is unusable.
    #[error("option table: row {row}: {why}")]
    Row {
        /// The row's position, counting from 1.
        row: usize,
        /// What is wrong with it.
        why: String,
    },
    /// Two rows name the same instance type.
    #[error("option table: instance type {0} is listed twice")]
    Duplicate(String),
}

#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    option: Vec<RawRow>,
}

/// A row as written. `gpu_class` and `gpus` are the GPU facts rows carried before the
/// facts moved into the crate: read as any value, so an old file still parses, and never
/// used.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    instance_type: String,
    gpu_class: Option<toml::Value>,
    gpus: Option<toml::Value>,
}

impl OptionTable {
    /// Reads the `[[option]]` rows of `text`, a TOML document; keys outside `option` are
    /// left to the file's other readers. A row is
    ///
    /// ```toml
    /// [[option]]
    /// instance_type = "gpu_1x_a10"
    /// ```
    ///
    /// A row that also carries `gpu_class` or `gpus` parses as the row without them and
    /// logs one warning naming its instance type, once the whole table is accepted.
    ///
    /// # Errors
    ///
    /// [`OptionsError`] naming the row that broke a rule.
    pub fn parse(text: &str) -> Result<Self, OptionsError> {
        let document: Document =
            toml::from_str(text).map_err(|err| OptionsError::Syntax(cut(&err.to_string(), 400)))?;
        if document.option.is_empty() {
            return Err(OptionsError::NoRows);
        }
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        let mut old_shape = Vec::new();
        for (index, raw) in document.option.into_iter().enumerate() {
            let carries_gpu_facts = raw.gpu_class.is_some() || raw.gpus.is_some();
            let row = row(index + 1, raw.instance_type)?;
            if !seen.insert(row.instance_type.clone()) {
                return Err(OptionsError::Duplicate(row.instance_type));
            }
            old_shape.extend(carries_gpu_facts.then(|| row.instance_type.clone()));
            rows.push(row);
        }
        for name in old_shape {
            tracing::warn!(
                instance_type = %name,
                "option row {name}: gpu_class and gpus are ignored; the GPU facts of an \
                 instance type come from js-provider-lambdalabs, so remove them from the row"
            );
        }
        Ok(Self::from_rows(rows))
    }

    /// A table of `rows`, which the caller has checked: each row's instance type is
    /// 1 to 64 lower case letters, digits or `_`, no two rows name one type, and there is
    /// at least one row ([`OptionTable::parse`] and the `lambdalabs` kind check as much).
    pub fn from_rows(rows: Vec<OptionRow>) -> Self {
        Self { rows }
    }

    /// A table with no row, for a caller that never launches (it has no instance type to
    /// offer): a tool that only reads or terminates.
    pub fn empty() -> Self {
        Self { rows: Vec::new() }
    }

    /// The rows.
    pub fn rows(&self) -> &[OptionRow] {
        &self.rows
    }

    /// The row for the instance type `name`.
    pub fn get(&self, name: &str) -> Option<&OptionRow> {
        self.rows.iter().find(|row| row.instance_type == name)
    }
}

/// Whether `name` is an instance-type name: 1 to 64 lower case letters, digits or `_`.
pub(crate) fn instance_type_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn row(number: usize, instance_type: String) -> Result<OptionRow, OptionsError> {
    match instance_type_name(&instance_type) {
        true => Ok(OptionRow { instance_type }),
        false => Err(OptionsError::Row {
            row: number,
            why: "instance_type must be 1 to 64 lower case letters, digits or '_'".to_owned(),
        }),
    }
}
