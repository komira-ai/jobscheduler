//! The live checks of the provider: what only the account can say, asked with reads and
//! never with a write. A key or a ruleset that a launch would make is reported as a note,
//! not made.

use std::collections::BTreeSet;

use js_elastic::CheckResult;

use super::{KeyPick, LambdaProvider, PrepareError};
use crate::error::ApiError;
use crate::model::{FirewallRuleset, InstanceTypeOffer};
use crate::select;

impl LambdaProvider {
    /// The checks of one pass, in the order of the design: the `types` rows against the
    /// catalog, the ssh key, the firewall. Each calls the API with list calls only.
    pub(super) fn live_checks(&self) -> Vec<CheckResult> {
        let types = self.api.instance_types();
        vec![
            self.types_check(&types),
            self.ssh_key_check(),
            self.firewall_check(types.as_deref().ok()),
        ]
    }

    /// Each `types` row names a type of the catalog with the GPU count the row gives.
    fn types_check(&self, types: &Result<Vec<InstanceTypeOffer>, ApiError>) -> CheckResult {
        let faults = match types {
            Ok(types) => select::row_faults(&self.options, types),
            Err(error) => vec![error.to_string()],
        };
        match faults.is_empty() {
            true => CheckResult::held("types"),
            false => CheckResult::failed("types", format!("catalog: {}", faults.join("; "))),
        }
    }

    /// The key a launch would hold exists, by the rule of `ssh_key` but with a list and
    /// nothing else: an account with no key passes with a note, and the key is not made.
    fn ssh_key_check(&self) -> CheckResult {
        let keys = match self.api.list_ssh_keys() {
            Ok(keys) => keys,
            Err(error) => {
                return CheckResult::failed("ssh_key", format!("provider.ssh_key: {error}"));
            }
        };
        if let Some(name) = &self.config.ssh_key {
            return match keys.iter().any(|key| &key.name == name) {
                true => CheckResult::held("ssh_key"),
                false => CheckResult::failed(
                    "ssh_key",
                    format!("provider.ssh_key: key {name} is not on the account"),
                ),
            };
        }
        match self.pick_key(&keys) {
            Ok(KeyPick::Use(_)) => CheckResult::held("ssh_key"),
            Ok(KeyPick::Make(name)) => CheckResult::held_noting(
                "ssh_key",
                format!("account has 0 keys; a launch will create {name}"),
            ),
            Err(why) => CheckResult::failed("ssh_key", why.to_string()),
        }
    }

    /// In each region the pool launches into, the ruleset `js-<scheduler>-noinbound` is
    /// absent (a launch will make it) or holds no rule. Which regions: the configured
    /// ones, else those that offer a type with a `types` row, else any region.
    fn firewall_check(&self, types: Option<&[InstanceTypeOffer]>) -> CheckResult {
        let name = format!("js-{}-noinbound", self.config.scheduler.as_str());
        if name.len() > 64 {
            return CheckResult::failed("firewall", PrepareError::FirewallName.to_string());
        }
        let rulesets = match self.api.list_firewall_rulesets() {
            Ok(rulesets) => rulesets,
            Err(error) => return CheckResult::failed("firewall", format!("firewall: {error}")),
        };
        let regions = self.firewall_regions(types);
        let ours: Vec<&FirewallRuleset> = rulesets
            .iter()
            .filter(|ruleset| ruleset.name == name)
            .filter(|ruleset| regions.is_empty() || regions.contains(&ruleset.region))
            .collect();
        if let Some(full) = ours.iter().find(|ruleset| ruleset.rules > 0) {
            let refusal = PrepareError::FirewallRules {
                name,
                rules: full.rules,
            };
            return CheckResult::failed("firewall", format!("{refusal} (region {})", full.region));
        }
        let missing: Vec<&str> = regions
            .iter()
            .filter(|region| ours.iter().all(|ruleset| &ruleset.region != *region))
            .map(String::as_str)
            .collect();
        match missing.is_empty() {
            true => CheckResult::held("firewall"),
            false => CheckResult::held_noting(
                "firewall",
                format!("{name} will be created in {}", missing.join(", ")),
            ),
        }
    }

    /// The regions a launch can go to: empty when they are not known.
    fn firewall_regions(&self, types: Option<&[InstanceTypeOffer]>) -> BTreeSet<String> {
        if let Some(only) = &self.config.regions {
            return only.iter().cloned().collect();
        }
        types
            .unwrap_or_default()
            .iter()
            .filter(|kind| self.options.get(&kind.name).is_some())
            .flat_map(|kind| kind.regions.iter().map(|region| region.name.clone()))
            .collect()
    }
}
