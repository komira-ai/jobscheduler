//! Makes one named cloud of the pool refuse one instance type in one region for quota.

use super::Action;
use crate::cell::Cell;

/// Makes the cloud named `provider` refuse every acquire of `instance_type` in `region` for
/// `quota` from now on ([`Cell::quota_refuse`]); a region is a zone of the cloud's offers,
/// as the server reads them. Nothing for a cell with no cloud of that name.
#[derive(Clone, Debug)]
pub struct QuotaRefuse {
    provider: String,
    instance_type: String,
    region: String,
}

/// Makes the cloud named `provider` refuse `instance_type` in `region` for quota now.
#[must_use]
pub fn quota_refuse(provider: &str, instance_type: &str, region: &str) -> QuotaRefuse {
    QuotaRefuse {
        provider: provider.to_owned(),
        instance_type: instance_type.to_owned(),
        region: region.to_owned(),
    }
}

impl Action for QuotaRefuse {
    fn run(&self, cell: &mut Cell) {
        let region = Some(self.region.as_str());
        cell.quota_refuse(&self.provider, &self.instance_type, region);
    }
}
