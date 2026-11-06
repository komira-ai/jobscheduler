//! Which running-vCPU quota an instance type counts against. EC2 states its on-demand and
//! spot limits in vCPUs per group of instance families (Amazon EC2 instance type quotas,
//! in the EC2 instance types guide); the group is chosen from the start of the family's
//! name. The number read is information only: it never removes, delays or marks an offer
//! (no check before a launch; a quota refusal at launch is the engine's to handle).

use crate::model::InstanceType;

/// A group of instance families that share one running-vCPU quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QuotaFamily {
    /// Standard (A, C, D, H, I, M, R, T, Z).
    Standard,
    /// DL.
    Dl,
    /// F.
    F,
    /// G and VT.
    GAndVt,
    /// HPC (on-demand only: no spot quota is listed for it).
    Hpc,
    /// High Memory (`u-` families; on-demand only).
    HighMemory,
    /// Inf.
    Inf,
    /// P.
    P,
    /// Trn.
    Trn,
    /// X.
    X,
}

/// The rows: a family-name prefix and the quota group it belongs to, matched longest first (so `inf2` is Inf, not I, and `dl1` is DL, not D).
/// Each row names the quota as the EC2 instance type quotas page does, with its on-demand
/// and spot quota codes.
const ROWS: [(&str, QuotaFamily); 19] = [
    // "Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances" (L-1216C47A);
    // "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests" (L-34B43A08).
    ("a", QuotaFamily::Standard),
    // The same Standard quotas (C).
    ("c", QuotaFamily::Standard),
    // The same Standard quotas (D).
    ("d", QuotaFamily::Standard),
    // The same Standard quotas (H).
    ("h", QuotaFamily::Standard),
    // The same Standard quotas (I, which `im4gn` and `is4gen` begin with too).
    ("i", QuotaFamily::Standard),
    // The same Standard quotas (M).
    ("m", QuotaFamily::Standard),
    // The same Standard quotas (R).
    ("r", QuotaFamily::Standard),
    // The same Standard quotas (T).
    ("t", QuotaFamily::Standard),
    // The same Standard quotas (Z).
    ("z", QuotaFamily::Standard),
    // "Running On-Demand DL instances" (L-6E869C2A); "All DL Spot Instance Requests"
    // (L-85EED4F7).
    ("dl", QuotaFamily::Dl),
    // "Running On-Demand F instances" (L-74FC7D96); "All F Spot Instance Requests"
    // (L-88CF9481).
    ("f", QuotaFamily::F),
    // "Running On-Demand G and VT instances" (L-DB2E81BA); "All G and VT Spot Instance
    // Requests" (L-3819A6DF).
    ("g", QuotaFamily::GAndVt),
    // The same G and VT quotas (VT).
    ("vt", QuotaFamily::GAndVt),
    // "Running On-Demand HPC instances" (L-F7808C92).
    ("hpc", QuotaFamily::Hpc),
    // "Running On-Demand High Memory instances" (L-43DA4232).
    ("u", QuotaFamily::HighMemory),
    // "Running On-Demand Inf instances" (L-1945791B); "All Inf Spot Instance Requests"
    // (L-B5D1601B).
    ("inf", QuotaFamily::Inf),
    // "Running On-Demand P instances" (L-417A185B); "All P Spot Instance Requests"
    // (L-7212CCBC).
    ("p", QuotaFamily::P),
    // "Running On-Demand Trn instances" (L-2C3B7624); "All Trn Spot Instance Requests"
    // (L-6B0D517C).
    ("trn", QuotaFamily::Trn),
    // "Running On-Demand X instances" (L-7295265B); "All X Spot Instance Requests"
    // (L-E3A00192).
    ("x", QuotaFamily::X),
];

/// The quota group of `instance_type`: the row whose prefix is the longest that its
/// family's name starts with. `None` for a family no row starts.
pub(crate) fn quota_family(instance_type: &InstanceType) -> Option<QuotaFamily> {
    let family = instance_type.family();
    ROWS.iter()
        .filter(|(prefix, _)| family.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, group)| *group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(name: &str) -> Option<QuotaFamily> {
        quota_family(&InstanceType(name.to_owned()))
    }

    /// Asserts that every row of the table is reached by a family that starts with its
    /// prefix and nothing longer, and that the longest prefix wins where two rows start a
    /// name: `inf2` is Inf and `i4i` Standard, `dl1` is DL and `d3` Standard, `hpc7g` is
    /// HPC and `h1` Standard, `trn1` is Trn and `t3` Standard, `vt1` and `g5` are G and
    /// VT, `p6e-gb200` is P. A family no row starts (`mac2` is not one, as `m` starts it;
    /// `q1` is) has none. Catches a row that maps to another quota, or a match that takes
    /// the first prefix instead of the longest.
    #[test]
    fn each_family_counts_against_its_documented_quota() {
        for (prefix, group) in ROWS {
            assert_eq!(of(&format!("{prefix}9.large")), Some(group), "{prefix}");
        }
        let cases = [
            ("inf2.xlarge", QuotaFamily::Inf),
            ("i4i.large", QuotaFamily::Standard),
            ("im4gn.large", QuotaFamily::Standard),
            ("dl1.24xlarge", QuotaFamily::Dl),
            ("d3.xlarge", QuotaFamily::Standard),
            ("hpc7g.16xlarge", QuotaFamily::Hpc),
            ("h1.2xlarge", QuotaFamily::Standard),
            ("trn1.32xlarge", QuotaFamily::Trn),
            ("t3.micro", QuotaFamily::Standard),
            ("vt1.3xlarge", QuotaFamily::GAndVt),
            ("g5.xlarge", QuotaFamily::GAndVt),
            ("p6e-gb200.36xlarge", QuotaFamily::P),
            ("u-6tb1.metal", QuotaFamily::HighMemory),
            ("x2idn.16xlarge", QuotaFamily::X),
            ("f2.12xlarge", QuotaFamily::F),
            ("p5", QuotaFamily::P),
        ];
        for (name, group) in cases {
            assert_eq!(of(name), Some(group), "{name}");
        }
        assert_eq!(of("q1.large"), None);
        assert_eq!(of(""), None);
    }
}
