//! One table of the configuration file and the readers of its fields: each refusal names
//! the table and the field and never repeats the value.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use js_types::{Admission, MEMORY_PERCENT_KEY, MemoryPercent, ResourceClass};

use crate::agent_pool::MIXED_UNSERVED;
use crate::config::parse_duration;
use crate::error::StartupError;

/// One table of the file, and where it is, for the messages.
pub(super) struct Section {
    pub(super) map: toml::Table,
    pub(super) head: String,
}

impl Section {
    pub(super) fn open(map: toml::Table, head: String) -> Self {
        Self { map, head }
    }

    /// A refusal of `key` of this table.
    pub(super) fn fail(&self, key: &str, reason: &str) -> StartupError {
        StartupError::at(format!("{}{key}", self.head), reason)
    }

    /// Refuses a class that books part of a host: this server serves whole hosts only.
    pub(super) fn class_served(&self, class: &ResourceClass) -> Result<(), StartupError> {
        match class.admission() {
            Admission::Book(_) => Err(self.fail("class", MIXED_UNSERVED)),
            _ => Ok(()),
        }
    }

    /// Refuses the first field that is not one of `known`.
    pub(super) fn only(&self, known: &[&str]) -> Result<(), StartupError> {
        for key in self.map.keys() {
            if !known.contains(&key.as_str()) {
                let reason = format!("unknown field; known: {}", known.join(", "));
                return Err(self.fail(key, &reason));
            }
        }
        Ok(())
    }

    pub(super) fn string(&mut self, key: &str) -> Result<Option<String>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(self.fail(key, "must be a string")),
        }
    }

    pub(super) fn required_string(&mut self, key: &str) -> Result<String, StartupError> {
        match self.string(key)? {
            Some(text) => Ok(text),
            None => Err(self.fail(key, "required")),
        }
    }

    pub(super) fn path(&mut self, key: &str) -> Result<Option<PathBuf>, StartupError> {
        Ok(self.string(key)?.map(PathBuf::from))
    }

    pub(super) fn required_path(&mut self, key: &str) -> Result<PathBuf, StartupError> {
        match self.path(key)? {
            Some(path) => Ok(path),
            None => Err(self.fail(key, "required")),
        }
    }

    pub(super) fn integer(&mut self, key: &str) -> Result<Option<u64>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Integer(number)) => match u64::try_from(number) {
                Ok(number) => Ok(Some(number)),
                Err(_) => Err(self.fail(key, "must not be negative")),
            },
            Some(_) => Err(self.fail(key, "must be an integer")),
        }
    }

    /// The integer `key` when the file gives it, which must be from `low` to `high`.
    pub(super) fn bounded_given(
        &mut self,
        key: &str,
        low: u64,
        high: u64,
    ) -> Result<Option<u64>, StartupError> {
        let Some(number) = self.integer(key)? else {
            return Ok(None);
        };
        match (low..=high).contains(&number) {
            true => Ok(Some(number)),
            false => Err(self.fail(key, &format!("must be from {low} to {high}"))),
        }
    }

    /// The integer `key`, or `default`, which must be from `low` to `high`.
    pub(super) fn bounded(
        &mut self,
        key: &str,
        low: u64,
        high: u64,
        default: u64,
    ) -> Result<u64, StartupError> {
        Ok(self.bounded_given(key, low, high)?.unwrap_or(default))
    }

    pub(super) fn boolean(&mut self, key: &str) -> Result<Option<bool>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Boolean(flag)) => Ok(Some(flag)),
            Some(_) => Err(self.fail(key, "must be true or false")),
        }
    }

    pub(super) fn duration(&mut self, key: &str) -> Result<Option<Duration>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match parse_duration(&text) {
            Ok(duration) => Ok(Some(duration)),
            Err(_) => Err(self.fail(key, "must be a positive duration such as 600s, 10m or 1h")),
        }
    }

    pub(super) fn duration_or(
        &mut self,
        key: &str,
        default: Duration,
    ) -> Result<Duration, StartupError> {
        Ok(self.duration(key)?.unwrap_or(default))
    }

    pub(super) fn address(&mut self, key: &str) -> Result<Option<SocketAddr>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match text.parse::<SocketAddr>() {
            Ok(address) => Ok(Some(address)),
            Err(_) => Err(self.fail(key, "must be an address such as 127.0.0.1:7300")),
        }
    }

    pub(super) fn table(&mut self, key: &str) -> Result<Option<toml::Table>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Table(table)) => Ok(Some(table)),
            Some(_) => Err(self.fail(key, "must be a table")),
        }
    }

    pub(super) fn tables(&mut self, key: &str) -> Result<Vec<toml::Table>, StartupError> {
        let must = format!("must be an array of tables, written [[{key}]]");
        let Some(value) = self.map.remove(key) else {
            return Ok(Vec::new());
        };
        let toml::Value::Array(items) = value else {
            return Err(self.fail(key, &must));
        };
        let mut tables = Vec::new();
        for item in items {
            let toml::Value::Table(table) = item else {
                return Err(self.fail(key, &must));
            };
            tables.push(table);
        }
        Ok(tables)
    }

    /// The list of names `key` when the table gives it: an array of strings, in the file's
    /// order, with at least one, none empty and none twice. A refusal names the repeated
    /// name: names are not secrets.
    pub(super) fn names(&mut self, key: &str) -> Result<Option<Vec<String>>, StartupError> {
        let Some(value) = self.map.remove(key) else {
            return Ok(None);
        };
        let toml::Value::Array(items) = value else {
            return Err(self.fail(key, "must be an array of strings"));
        };
        let mut names: Vec<String> = Vec::new();
        for item in items {
            let toml::Value::String(name) = item else {
                return Err(self.fail(key, "must be an array of strings"));
            };
            if name.is_empty() {
                return Err(self.fail(key, "holds an empty name"));
            }
            if names.contains(&name) {
                return Err(self.fail(key, &format!("names {name:?} twice")));
            }
            names.push(name);
        }
        match names.is_empty() {
            true => Err(self.fail(key, "must name at least one")),
            false => Ok(Some(names)),
        }
    }

    /// The pool's `memory_schedulable_percent` when the table gives it: a whole number
    /// from 1 to 100.
    pub(super) fn memory_percent(&mut self) -> Result<Option<MemoryPercent>, StartupError> {
        let Some(percent) = self.integer(MEMORY_PERCENT_KEY)? else {
            return Ok(None);
        };
        let bounded = MemoryPercent::new(percent);
        bounded
            .map(Some)
            .map_err(|_| self.fail(MEMORY_PERCENT_KEY, "must be from 1 to 100"))
    }

    /// A spend in cents: a string holding a decimal with at most two places, above zero.
    pub(super) fn spend(&mut self, key: &str) -> Result<Option<u64>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match cents(&text) {
            None => Err(self.fail(
                key,
                "must be a decimal with at most two places, such as \"12.00\"",
            )),
            Some(0) => Err(self.fail(key, "must be above zero")),
            Some(amount) => Ok(Some(amount)),
        }
    }
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn digits(part: &str) -> bool {
    part.bytes().all(is_digit)
}

/// `text` as cents: digits, then optionally a point and one or two digits.
fn cents(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let valid = [
        !whole.is_empty(),
        fraction.len() <= 2,
        !text.ends_with('.'),
        digits(whole),
        digits(fraction),
    ];
    match valid.contains(&false) {
        true => None,
        false => format!("{whole}{fraction:0<2}").parse().ok(),
    }
}
