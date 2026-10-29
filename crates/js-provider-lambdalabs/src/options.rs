//! The `[[option]]` rows of the configuration: which Lambda instance types the provider
//! may use, and what GPUs each one brings. Parsing and checking only; nothing chooses
//! among the rows here.
//!
//! The row fields (`instance_type`, `gpu_class`, `gpus`) are not yet the shape of the
//! design's `[[provider.option]]` table (`shape`, `id`, `facts`). They are to be aligned
//! with it before anything reads the table beyond parsing.

use std::collections::BTreeSet;

use js_elastic::GpuClass;
use serde::Deserialize;

use crate::error::cut;

/// One instance type the provider may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    /// Lambda's name for the type, such as `gpu_1x_a10`.
    pub instance_type: String,
    /// The class the agent on this type reports for its GPUs.
    pub gpu_class: GpuClass,
    /// How many GPUs the type has (at least 1).
    pub gpus: u32,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    instance_type: String,
    gpu_class: String,
    gpus: u32,
}

impl OptionTable {
    /// Reads the `[[option]]` rows of `text`, a TOML document; keys outside `option` are
    /// left to the file's other readers. A row is
    ///
    /// ```toml
    /// [[option]]
    /// instance_type = "gpu_1x_a10"
    /// gpu_class = "nvidia-a10"
    /// gpus = 1
    /// ```
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
        for (index, raw) in document.option.into_iter().enumerate() {
            let row = row(index + 1, raw)?;
            if !seen.insert(row.instance_type.clone()) {
                return Err(OptionsError::Duplicate(row.instance_type));
            }
            rows.push(row);
        }
        Ok(Self::from_rows(rows))
    }

    /// A table of `rows`, which the caller has checked: each row's instance type is
    /// 1 to 64 lower case letters, digits or `_`, no two rows name one type, and there is
    /// at least one row ([`OptionTable::parse`] and the `lambdalabs` kind check as much).
    pub fn from_rows(rows: Vec<OptionRow>) -> Self {
        Self { rows }
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

fn row(number: usize, raw: RawRow) -> Result<OptionRow, OptionsError> {
    let refuse = |why: String| OptionsError::Row { row: number, why };
    let named = !raw.instance_type.is_empty()
        && raw.instance_type.len() <= 64
        && raw
            .instance_type
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !named {
        return Err(refuse(
            "instance_type must be 1 to 64 lower case letters, digits or '_'".to_owned(),
        ));
    }
    if raw.gpus == 0 {
        return Err(refuse("gpus must be at least 1".to_owned()));
    }
    let gpu_class =
        GpuClass::parse(&raw.gpu_class).map_err(|why| refuse(format!("gpu_class {why}")))?;
    Ok(OptionRow {
        instance_type: raw.instance_type,
        gpu_class,
        gpus: raw.gpus,
    })
}
