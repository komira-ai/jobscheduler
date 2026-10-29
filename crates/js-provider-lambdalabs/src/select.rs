//! Choosing where to launch: the instance types and regions that fit a request, best first.

use crate::elastic::{Candidate, GpuRequest};
use crate::model::{Architecture, InstanceTypeOffer};
use crate::options::OptionTable;

const GIB: u64 = 1 << 30;

/// The (type, region) pairs that fit `request`, in the order to try them: cheapest type
/// first, then fewest GPUs, then name, then region name. A type is a candidate when
///
/// - it is a row of `table` with the request's GPU class and the GPU count the API gives
///   for it (a row that disagrees with the API is a configuration error and is skipped),
/// - it is x86_64 (the agent is released for no other architecture),
/// - it has at least the requested GPUs, CPU and memory,
/// - and it has capacity in the region now.
///
/// The price orders and never excludes. No candidate means no capacity.
pub(crate) fn candidates(
    table: &OptionTable,
    offers: &[InstanceTypeOffer],
    request: &GpuRequest,
) -> Vec<Candidate> {
    let mut fits: Vec<&InstanceTypeOffer> = offers
        .iter()
        .filter(|offer| {
            table.get(&offer.name).is_some_and(|row| {
                row.gpu_class == request.gpu_class
                    && row.gpus == offer.gpus
                    && offer.architecture == Architecture::X86_64
                    && offer.gpus >= request.gpus
                    && u64::from(offer.vcpus) * 1000 >= request.cpu_millis
                    && u64::from(offer.memory_gib) * GIB >= request.memory_bytes
            })
        })
        .collect();
    fits.sort_by(|a, b| {
        (a.price_cents_per_hour, a.gpus, &a.name).cmp(&(b.price_cents_per_hour, b.gpus, &b.name))
    });
    fits.into_iter()
        .flat_map(|offer| {
            let mut regions: Vec<&str> = offer.regions.iter().map(|r| r.name.as_str()).collect();
            regions.sort_unstable();
            regions.dedup();
            regions.into_iter().map(|region| Candidate {
                instance_type: offer.name.clone(),
                region: region.to_owned(),
                price_cents_per_hour: offer.price_cents_per_hour,
                gpus: offer.gpus,
            })
        })
        .collect()
}
