//! A catalog whose blocking API call panics: the provider answers an error and goes on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_elastic::ProviderError;
use js_provider_lambdalabs::{
    ApiError, FakeLambda, FirewallRuleset, Instance, InstanceId, InstanceTypeOffer, LambdaApi,
    LambdaProvider, LaunchRequest, ManualClock, OptionTable, SshKey,
};

use crate::rig::{TABLE, catalog, live};

/// The in-memory account, except that the first read of the instance types panics.
struct PanicsOnce {
    inner: FakeLambda,
    armed: AtomicBool,
}

impl LambdaApi for PanicsOnce {
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        assert!(!self.armed.swap(false, Ordering::SeqCst), "the call died");
        self.inner.instance_types()
    }

    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        self.inner.list_instances()
    }

    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError> {
        self.inner.get_instance(id)
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError> {
        self.inner.launch(request)
    }

    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError> {
        self.inner.terminate(id)
    }

    fn list_firewall_rulesets(&self) -> Result<Vec<FirewallRuleset>, ApiError> {
        self.inner.list_firewall_rulesets()
    }

    fn create_firewall_ruleset(
        &self,
        region: &str,
        name: &str,
    ) -> Result<FirewallRuleset, ApiError> {
        self.inner.create_firewall_ruleset(region, name)
    }

    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        self.inner.list_ssh_keys()
    }

    fn create_ssh_key(&self, name: &str) -> Result<SshKey, ApiError> {
        self.inner.create_ssh_key(name)
    }
}

/// Asserts that a catalog whose blocking call panics answers exactly
/// `ProviderError::new("the catalog call did not finish")`, and that the next catalog call
/// on the same provider returns the account's offers. Catches a panic that unwinds into the
/// caller, or one that leaves the provider unusable.
#[test]
fn a_catalog_call_that_panics_is_an_error_and_the_provider_goes_on() {
    let fake = FakeLambda::new();
    fake.set_offers(vec![crate::rig::offer(
        "gpu_1x_sample",
        75,
        1,
        js_provider_lambdalabs::Architecture::X86_64,
        &["region-a"],
    )]);
    let api = Arc::new(PanicsOnce {
        inner: fake,
        armed: AtomicBool::new(true),
    });
    let provider = LambdaProvider::new(
        api,
        Arc::new(ManualClock::new()),
        OptionTable::parse(TABLE).unwrap(),
        live(5),
    );
    assert_eq!(
        catalog(&provider),
        Err(ProviderError::new("the catalog call did not finish"))
    );
    let offers = catalog(&provider).unwrap();
    assert_eq!(offers.len(), 1);
}
