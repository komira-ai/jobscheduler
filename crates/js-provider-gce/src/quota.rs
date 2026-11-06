//! Which of a region's quota metrics a machine counts against, read from `regions.get`.
//! Compute Engine limits the vCPUs of a machine series and, separately, the GPUs of each
//! GPU model, with the standard and the preemptible (Spot) limits as separate metrics
//! (Compute Engine resource usage and quotas). The numbers are information only: they never
//! remove, delay or mark an offer (no check before a launch; a quota refusal at launch is
//! the engine's to handle).
//!
//! A Spot machine counts against the preemptible metric where the project has preemptible
//! quota, and against the standard metric where it has none ("In regions where you don't
//! have preemptible quota, you can use standard quota to launch Spot VMs", Spot VMs): a
//! Spot offer states the preemptible metric when the region lists it with a limit above
//! zero, or when there is no standard metric to state, and the standard one otherwise.

use std::collections::BTreeMap;

use js_elastic::{Quota, QuotaLimit};

use crate::api::PurchaseKind;
use crate::model::{AcceleratorTypeName, MachineTypeName, QuotaMetric};

/// The vCPU metrics: a machine type name prefix, its standard metric and its preemptible
/// one. A series with no row (the newer series, limited per VM family through Cloud Quotas
/// rather than a `regions.get` metric, and the GPU series A3, A4, G2 and G4, which "only
/// need to have the required GPU quotas") states no vCPU limit.
pub(crate) const CPU_ROWS: [(&str, &str, Option<&str>); 6] = [
    // E2: "E2 and N1 machine types share a CPU quota pool", `CPUS`; Spot VMs count against
    // `PREEMPTIBLE_CPUS`.
    ("e2-", "CPUS", Some("PREEMPTIBLE_CPUS")),
    // N1: the same pool as E2, `CPUS` and `PREEMPTIBLE_CPUS`.
    ("n1-", "CPUS", Some("PREEMPTIBLE_CPUS")),
    // N2: its own pool, `N2_CPUS`.
    ("n2-", "N2_CPUS", None),
    // C2: its own pool, `C2_CPUS`.
    ("c2-", "C2_CPUS", None),
    // T2D: its own pool, `T2D_CPUS`.
    ("t2d-", "T2D_CPUS", None),
    // M3: its own pool, `M3_CPUS`.
    ("m3-", "M3_CPUS", None),
];

/// The GPU metrics: an accelerator type name, its standard metric and its preemptible one,
/// as the GPU quota table of resource usage and quotas names them. `None` where the table
/// names no such metric: the GPUs with no standard quota (A3 Ultra, A4), and those whose
/// standard quota the table names only as a Cloud Quotas GPU family (H100, H100 Mega, L4,
/// RTX PRO 6000), which `regions.get` does not list under that name.
pub(crate) const GPU_ROWS: [(&str, Option<&str>, Option<&str>); 14] = [
    // A2 Standard, A100 40GB.
    (
        "nvidia-tesla-a100",
        Some("NVIDIA_A100_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_A100_GPUS"),
    ),
    // A2 Ultra, A100 80GB.
    (
        "nvidia-a100-80gb",
        Some("NVIDIA_A100_80GB_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_A100_80GB_GPUS"),
    ),
    // A3 High and A3 Edge, H100: preemptible only (the standard quota is a GPU family).
    (
        "nvidia-h100-80gb",
        None,
        Some("PREEMPTIBLE_NVIDIA_H100_GPUS"),
    ),
    // A3 Mega, H100 Mega: preemptible only (the standard quota is a GPU family).
    (
        "nvidia-h100-mega-80gb",
        None,
        Some("PREEMPTIBLE_NVIDIA_H100_MEGA_GPUS"),
    ),
    // A3 Ultra, H200: no standard quota.
    (
        "nvidia-h200-141gb",
        None,
        Some("PREEMPTIBLE_NVIDIA_H200_GPUS"),
    ),
    // A4, B200: no standard quota.
    ("nvidia-b200", None, Some("PREEMPTIBLE_NVIDIA_B200_GPUS")),
    // G2, L4: preemptible only (the standard quota is a GPU family).
    ("nvidia-l4", None, Some("PREEMPTIBLE_NVIDIA_L4_GPUS")),
    // G2, L4 virtual workstation.
    (
        "nvidia-l4-vws",
        Some("NVIDIA_L4_VWS_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_L4_VWS_GPUS"),
    ),
    // G4, RTX PRO 6000: preemptible only (the standard quota is a GPU family).
    (
        "nvidia-rtx-pro-6000",
        None,
        Some("PREEMPTIBLE_NVIDIA_RTX_PRO_6000_GPUS"),
    ),
    // G4, RTX PRO 6000 virtual workstation.
    (
        "nvidia-rtx-pro-6000-vws",
        Some("NVIDIA_RTX_PRO_6000_VWS_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_RTX_PRO_6000_VWS_GPUS"),
    ),
    // N1 with T4.
    (
        "nvidia-tesla-t4",
        Some("NVIDIA_T4_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_T4_GPUS"),
    ),
    // N1 with T4 virtual workstation.
    (
        "nvidia-tesla-t4-vws",
        Some("NVIDIA_T4_VWS_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_T4_VWS_GPUS"),
    ),
    // N1 with V100.
    (
        "nvidia-tesla-v100",
        Some("NVIDIA_V100_GPUS"),
        Some("PREEMPTIBLE_NVIDIA_V100_GPUS"),
    ),
    // N1 with P4: the table names no preemptible metric for it.
    ("nvidia-tesla-p4", Some("NVIDIA_P4_GPUS"), None),
];

/// The quotas of one region, by metric.
#[derive(Debug, Default)]
pub(crate) struct RegionQuotas(BTreeMap<String, QuotaLimit>);

impl RegionQuotas {
    /// The quotas of `metrics` (`regions.get` lists each metric once). A number beyond 32
    /// bits is read as the largest that fits: it is information, and no limit is that
    /// large.
    pub(crate) fn read(metrics: &[QuotaMetric]) -> Self {
        let fit = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
        Self(
            metrics
                .iter()
                .map(|metric| {
                    let limit = QuotaLimit {
                        limit: fit(metric.limit),
                        in_use: metric.usage.map(fit),
                    };
                    (metric.metric.clone(), limit)
                })
                .collect(),
        )
    }

    fn get(&self, metric: Option<&str>) -> Option<QuotaLimit> {
        metric.and_then(|metric| self.0.get(metric).copied())
    }

    /// The limit of the standard or the preemptible metric that a machine bought as `kind`
    /// counts against.
    fn limit(
        &self,
        standard: Option<&str>,
        preemptible: Option<&str>,
        kind: PurchaseKind,
    ) -> Option<QuotaLimit> {
        let standard = self.get(standard);
        match (kind, self.get(preemptible), standard) {
            (PurchaseKind::OnDemand, _, standard) => standard,
            (PurchaseKind::Spot, Some(spot), Some(standard)) if spot.limit == 0 => Some(standard),
            (PurchaseKind::Spot, Some(spot), _) => Some(spot),
            (PurchaseKind::Spot, None, standard) => standard,
        }
    }

    /// The quota a machine of the type `machine` with `accelerator` bought as `kind`
    /// counts against: its series' vCPUs and its accelerator's GPUs. `None` when the region
    /// lists neither.
    pub(crate) fn quota_for(
        &self,
        machine: &MachineTypeName,
        accelerator: Option<&AcceleratorTypeName>,
        kind: PurchaseKind,
    ) -> Option<Quota> {
        let vcpus = CPU_ROWS
            .iter()
            .find(|(prefix, _, _)| machine.as_str().starts_with(prefix))
            .and_then(|(_, standard, preemptible)| self.limit(Some(*standard), *preemptible, kind));
        let accelerators = accelerator
            .and_then(|name| GPU_ROWS.iter().find(|(row, _, _)| *row == name.as_str()))
            .and_then(|(_, standard, preemptible)| self.limit(*standard, *preemptible, kind));
        match (vcpus, accelerators) {
            (None, None) => None,
            (vcpus, accelerators) => Some(Quota {
                vcpus,
                accelerators,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(name: &str, limit: u64, usage: Option<u64>) -> QuotaMetric {
        QuotaMetric {
            metric: name.to_owned(),
            limit,
            usage,
        }
    }

    fn limit(limit: u32, in_use: Option<u32>) -> Option<QuotaLimit> {
        Some(QuotaLimit { limit, in_use })
    }

    /// A number of its own for each metric name in the tables: its bytes summed.
    fn number(name: &str) -> u32 {
        name.bytes().map(u32::from).sum()
    }

    /// Asserts that every row of the two tables is reached: each vCPU row by a type that
    /// starts with its prefix, and each GPU row by its accelerator name, the standard
    /// offer from the standard metric and the Spot offer from the preemptible one (or the
    /// standard one where the row has none), in a region that lists every metric with a
    /// number of its own. Catches a row that reads another metric, or standard and Spot
    /// read from one.
    #[test]
    fn each_row_reads_its_own_metrics() {
        let names = CPU_ROWS
            .iter()
            .flat_map(|(_, standard, preemptible)| [Some(*standard), *preemptible])
            .chain(
                GPU_ROWS
                    .iter()
                    .flat_map(|(_, standard, preemptible)| [*standard, *preemptible]),
            )
            .flatten();
        let metrics: Vec<QuotaMetric> = names
            .map(|name| {
                let number = u64::from(number(name));
                metric(name, number, Some(number / 2))
            })
            .collect();
        let quotas = RegionQuotas::read(&metrics);
        let of = |name: Option<&str>| {
            name.map(|name| QuotaLimit {
                limit: number(name),
                in_use: Some(number(name) / 2),
            })
        };
        for (prefix, standard, preemptible) in CPU_ROWS {
            let machine = MachineTypeName(format!("{prefix}standard-9"));
            let vcpus = |kind| quotas.quota_for(&machine, None, kind).and_then(|q| q.vcpus);
            assert_eq!(
                vcpus(PurchaseKind::OnDemand),
                of(Some(standard)),
                "{prefix}"
            );
            let spot = of(preemptible).or(of(Some(standard)));
            assert_eq!(vcpus(PurchaseKind::Spot), spot, "{prefix}");
        }
        let machine = MachineTypeName("q9-sample".to_owned());
        for (name, standard, preemptible) in GPU_ROWS {
            let accelerator = AcceleratorTypeName(name.to_owned());
            let accelerators = |kind| {
                quotas
                    .quota_for(&machine, Some(&accelerator), kind)
                    .and_then(|q| q.accelerators)
            };
            assert_eq!(accelerators(PurchaseKind::OnDemand), of(standard), "{name}");
            let spot = of(preemptible).or(of(standard));
            assert_eq!(accelerators(PurchaseKind::Spot), spot, "{name}");
        }
    }

    /// Asserts the Spot fallback: a preemptible limit of zero beside a standard metric
    /// states the standard one, a preemptible limit of zero with no standard metric states
    /// the zero; and that a type and an accelerator no row knows, or a region with no such
    /// metric, state no quota; and that a number beyond 32 bits is the largest that fits.
    #[test]
    fn spot_falls_back_to_the_standard_quota_where_it_has_none() {
        let quotas = RegionQuotas::read(&[
            metric("CPUS", 24, Some(5)),
            metric("PREEMPTIBLE_CPUS", 0, Some(0)),
            metric("PREEMPTIBLE_NVIDIA_B200_GPUS", 0, None),
            metric("NVIDIA_T4_GPUS", 1 << 40, Some(1 << 33)),
        ]);
        let n1 = MachineTypeName("n1-standard-8".to_owned());
        let a4 = MachineTypeName("a4-highgpu-8g".to_owned());
        let b200 = AcceleratorTypeName("nvidia-b200".to_owned());
        let t4 = AcceleratorTypeName("nvidia-tesla-t4".to_owned());
        assert_eq!(
            quotas.quota_for(&n1, Some(&t4), PurchaseKind::Spot),
            Some(Quota {
                vcpus: limit(24, Some(5)),
                accelerators: limit(u32::MAX, Some(u32::MAX)),
            })
        );
        assert_eq!(
            quotas.quota_for(&a4, Some(&b200), PurchaseKind::Spot),
            Some(Quota {
                vcpus: None,
                accelerators: limit(0, None),
            })
        );
        assert_eq!(
            quotas.quota_for(&a4, Some(&b200), PurchaseKind::OnDemand),
            None
        );
        let unknown = AcceleratorTypeName("nvidia-sample".to_owned());
        let z9 = MachineTypeName("z9-standard-8".to_owned());
        assert_eq!(
            quotas.quota_for(&z9, Some(&unknown), PurchaseKind::OnDemand),
            None
        );
        assert_eq!(quotas.quota_for(&z9, None, PurchaseKind::Spot), None);
        let empty = RegionQuotas::read(&[]);
        assert_eq!(
            empty.quota_for(&n1, Some(&t4), PurchaseKind::OnDemand),
            None
        );
    }
}
