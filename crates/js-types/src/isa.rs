//! The CPU vocabulary a request, an offer and a hello share: the architecture
//! ([`Arch`], with [`normalise_arch`] for text that may name one this crate does not know)
//! and the instruction-set level ([`IsaLevel`]): the x86-64 psABI microarchitecture
//! levels and the Armv8 architecture versions, each computed from a feature set in
//! kernel names.
//!
//! `arm64` is the one spelling of 64-bit Arm. `aarch64`, the name Rust and Linux give it,
//! is accepted wherever an architecture is parsed and becomes `arm64`.
//!
//! Levels are ordered within a family and cumulative: a node at `x86-64-v4` also
//! supports v1 to v3. The two families are never compared with each other.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The canonical name of 64-bit Arm.
const ARM64: &str = "arm64";
/// The name of 64-bit Arm that Rust (`std::env::consts::ARCH`) and Linux use, accepted as
/// an alias of [`ARM64`].
const AARCH64: &str = "aarch64";

/// `arch` in the one spelling: `aarch64` becomes `arm64`, and every other text, a name
/// this crate does not know and the empty text included, is returned as it is, so a host
/// that reports an architecture no release knows is not turned into another one.
#[must_use]
pub fn normalise_arch(arch: &str) -> &str {
    if arch == AARCH64 { ARM64 } else { arch }
}

/// A CPU architecture. On the wire it is its [name](Arch::name), and `aarch64` decodes as
/// [`Arch::Arm64`]; any other text fails to decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Arch {
    /// 64-bit x86: `x86_64`.
    #[serde(rename = "x86_64")]
    X86_64,
    /// 64-bit Arm: `arm64` (`aarch64` is read as it).
    #[serde(rename = "arm64", alias = "aarch64")]
    Arm64,
}

impl Arch {
    /// The canonical name: `x86_64` or `arm64`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64 => ARM64,
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that names no architecture this crate knows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown architecture {0:?}")]
pub struct UnknownArch(pub String);

impl FromStr for Arch {
    type Err = UnknownArch;

    /// `x86_64`, `arm64`, or the alias `aarch64` (read as `arm64`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match normalise_arch(s) {
            "x86_64" => Ok(Self::X86_64),
            ARM64 => Ok(Self::Arm64),
            _ => Err(UnknownArch(s.to_owned())),
        }
    }
}

/// An x86-64 microarchitecture level, as defined by the x86-64 psABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum X86Level {
    /// `x86-64-v1`: the baseline (CMOV, CX8, SSE2, ...).
    V1,
    /// `x86-64-v2`: adds SSE3 to SSE4.2, POPCNT and CX16.
    V2,
    /// `x86-64-v3`: adds AVX, AVX2, BMI1, BMI2, FMA and MOVBE.
    V3,
    /// `x86-64-v4`: adds AVX-512 F, BW, CD, DQ and VL.
    V4,
}

impl X86Level {
    /// Every level, lowest first.
    pub const ALL: [Self; 4] = [Self::V1, Self::V2, Self::V3, Self::V4];

    /// The features this level adds to the one below it, in `/proc/cpuinfo` flag names.
    ///
    /// The psABI names map to kernel flags as follows: SSE3 is `pni`, LAHF-SAHF is
    /// `lahf_lm`, LZCNT is `abm`, SCE is `syscall`, and OSFXSR is covered by `fxsr`.
    /// The kernel hides OSXSAVE; `xsave` stands in for it, and the kernel already
    /// clears `avx` and the AVX-512 flags when the OS has not enabled their registers.
    #[must_use]
    pub fn adds(self) -> &'static [&'static str] {
        match self {
            Self::V1 => &[
                "cmov", "cx8", "fpu", "fxsr", "mmx", "syscall", "sse", "sse2",
            ],
            Self::V2 => &[
                "cx16", "lahf_lm", "popcnt", "pni", "sse4_1", "sse4_2", "ssse3",
            ],
            Self::V3 => &[
                "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "abm", "movbe", "xsave",
            ],
            Self::V4 => &["avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"],
        }
    }

    /// The highest level whose features, and those of every level below it, are all
    /// present. `None` if even v1 is incomplete.
    #[must_use]
    pub fn highest(features: &BTreeSet<String>) -> Option<Self> {
        highest(&Self::ALL, features, |l| l.adds())
    }

    /// The psABI name, e.g. `x86-64-v3`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::V1 => "x86-64-v1",
            Self::V2 => "x86-64-v2",
            Self::V3 => "x86-64-v3",
            Self::V4 => "x86-64-v4",
        }
    }
}

/// An Armv8 architecture version, from `armv8.0-a` to `armv8.4-a`.
///
/// Arm defines no level ladder for user space, so a version here means "every
/// feature the Arm ARM makes mandatory at that version and that the kernel reports
/// as a hwcap is present". The ladder stops at `armv8.4-a`; later features are still
/// reported by name in the feature set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArmVersion {
    /// `armv8.0-a`.
    V8_0,
    /// `armv8.1-a`.
    V8_1,
    /// `armv8.2-a`.
    V8_2,
    /// `armv8.3-a`.
    V8_3,
    /// `armv8.4-a`.
    V8_4,
}

impl ArmVersion {
    /// Every version, lowest first.
    pub const ALL: [Self; 5] = [Self::V8_0, Self::V8_1, Self::V8_2, Self::V8_3, Self::V8_4];

    /// The hwcaps (kernel names) this version makes mandatory beyond the one below.
    ///
    /// 8.1: LSE (`atomics`), RDM (`asimdrdm`), CRC32. 8.2: DPB (`dcpop`). 8.3: PAuth
    /// (`paca`, `pacg`), JSCVT, FCMA, LRCPC. 8.4: DIT, LSE2 (`uscat`), LRCPC2
    /// (`ilrcpc`), FlagM.
    #[must_use]
    pub fn adds(self) -> &'static [&'static str] {
        match self {
            Self::V8_0 => &["fp", "asimd"],
            Self::V8_1 => &["atomics", "asimdrdm", "crc32"],
            Self::V8_2 => &["dcpop"],
            Self::V8_3 => &["paca", "pacg", "jscvt", "fcma", "lrcpc"],
            Self::V8_4 => &["dit", "uscat", "ilrcpc", "flagm"],
        }
    }

    /// The highest version whose mandatory hwcaps, and those of every version below
    /// it, are all present. `None` if even `armv8.0-a` is incomplete.
    #[must_use]
    pub fn highest(features: &BTreeSet<String>) -> Option<Self> {
        highest(&Self::ALL, features, |v| v.adds())
    }

    /// The compiler-style name, e.g. `armv8.2-a`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::V8_0 => "armv8.0-a",
            Self::V8_1 => "armv8.1-a",
            Self::V8_2 => "armv8.2-a",
            Self::V8_3 => "armv8.3-a",
            Self::V8_4 => "armv8.4-a",
        }
    }
}

fn highest<L: Copy>(
    ladder: &[L],
    features: &BTreeSet<String>,
    adds: impl Fn(L) -> &'static [&'static str],
) -> Option<L> {
    ladder
        .iter()
        .copied()
        .take_while(|&l| adds(l).iter().all(|f| features.contains(*f)))
        .last()
}

/// An instruction-set level in either family.
///
/// Deliberately not `Ord`: an x86-64 level and an Arm version are not comparable.
/// Use [`IsaLevel::satisfies`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IsaLevel {
    /// An x86-64 psABI level.
    X86_64(X86Level),
    /// An Armv8 version.
    Arm64(ArmVersion),
}

impl IsaLevel {
    /// Whether a node at this level serves a request for `required`: the same family,
    /// and this level at least as high.
    #[must_use]
    pub fn satisfies(self, required: Self) -> bool {
        match (self, required) {
            (Self::X86_64(have), Self::X86_64(want)) => have >= want,
            (Self::Arm64(have), Self::Arm64(want)) => have >= want,
            _ => false,
        }
    }

    /// This level and every level below it in its family, lowest first. This is the
    /// `isa_level` list of a node report.
    #[must_use]
    pub fn with_lower(self) -> Vec<Self> {
        match self {
            Self::X86_64(top) => X86Level::ALL
                .into_iter()
                .filter(|&l| l <= top)
                .map(Self::X86_64)
                .collect(),
            Self::Arm64(top) => ArmVersion::ALL
                .into_iter()
                .filter(|&v| v <= top)
                .map(Self::Arm64)
                .collect(),
        }
    }

    /// The highest level a capability value states: one level name, or several joined by
    /// `,` (`x86-64-v1,x86-64-v2,x86-64-v3`, the [`Self::with_lower`] list), with spaces
    /// around a name ignored. `None` when the text is empty, holds a name that is not a
    /// level, or names levels of both families: such a value states no level.
    #[must_use]
    pub fn highest_listed(text: &str) -> Option<Self> {
        let mut top: Option<Self> = None;
        for name in text.split(',') {
            let level: Self = name.trim().parse().ok()?;
            top = match top {
                None => Some(level),
                Some(seen) if seen.satisfies(level) => Some(seen),
                Some(seen) if level.satisfies(seen) => Some(level),
                Some(_) => return None,
            };
        }
        top
    }

    /// The name used in reports and requests.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64(l) => l.name(),
            Self::Arm64(v) => v.name(),
        }
    }
}

impl fmt::Display for IsaLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that names no level this crate knows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown ISA level {0:?}")]
pub struct UnknownIsaLevel(pub String);

impl FromStr for IsaLevel {
    type Err = UnknownIsaLevel;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        X86Level::ALL
            .into_iter()
            .map(Self::X86_64)
            .chain(ArmVersion::ALL.into_iter().map(Self::Arm64))
            .find(|l| l.name() == s)
            .ok_or_else(|| UnknownIsaLevel(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    /// Catches: a level computed as "this level's own features are present" without
    /// the levels below it. AVX-512 alone, with no SSE4.2, is not v4 (nor anything).
    #[test]
    fn levels_are_cumulative() {
        let mut f = set(X86Level::V4.adds());
        f.extend(set(X86Level::V3.adds()));
        assert_eq!(X86Level::highest(&f), None);
        for l in X86Level::ALL {
            f.extend(set(l.adds()));
        }
        assert_eq!(X86Level::highest(&f), Some(X86Level::V4));
    }

    /// Catches: a gap in the ladder that skips a missing level. With v2 incomplete,
    /// a full v3 and v4 feature set still yields v1.
    #[test]
    fn a_gap_stops_the_ladder() {
        let mut f = BTreeSet::new();
        for l in X86Level::ALL {
            f.extend(set(l.adds()));
        }
        f.remove("popcnt");
        assert_eq!(X86Level::highest(&f), Some(X86Level::V1));
    }

    /// Catches: comparison across families (an Arm version satisfying an x86-64
    /// request, or the reverse) and an at-least test written backwards.
    #[test]
    fn satisfies_is_at_least_within_a_family() {
        let v3 = IsaLevel::X86_64(X86Level::V3);
        let v4 = IsaLevel::X86_64(X86Level::V4);
        let a84 = IsaLevel::Arm64(ArmVersion::V8_4);
        assert!(v4.satisfies(v3));
        assert!(v3.satisfies(v3));
        assert!(!v3.satisfies(v4));
        assert!(!a84.satisfies(v3));
        assert!(!v4.satisfies(IsaLevel::Arm64(ArmVersion::V8_0)));
    }

    /// Catches: a name that does not round-trip, which would make a reported level
    /// unrequestable.
    #[test]
    fn names_round_trip() {
        for l in X86Level::ALL
            .map(IsaLevel::X86_64)
            .into_iter()
            .chain(ArmVersion::ALL.map(IsaLevel::Arm64))
        {
            assert_eq!(l.to_string().parse::<IsaLevel>(), Ok(l));
        }
        assert!("x86-64-v5".parse::<IsaLevel>().is_err());
        assert!("armv8.5-a".parse::<IsaLevel>().is_err());
    }

    /// Asserts that `aarch64` becomes `arm64` and that every other text, a name no
    /// release knows and the empty text included, is returned unchanged. Catches a
    /// normalisation that maps an unknown architecture to a known one, or drops it.
    #[test]
    fn an_unknown_arch_still_passes_through_normalise() {
        assert_eq!(normalise_arch("aarch64"), "arm64");
        assert_eq!(normalise_arch("arm64"), "arm64");
        assert_eq!(normalise_arch("x86_64"), "x86_64");
        assert_eq!(normalise_arch("sparc"), "sparc");
        assert_eq!(normalise_arch(""), "");
        assert_eq!(normalise_arch("AARCH64"), "AARCH64");
    }

    /// Asserts that the architecture parses from `x86_64`, `arm64` and the alias
    /// `aarch64`, always prints in the canonical spelling, and that any other text is
    /// refused naming the text as sent. Catches a parser that refuses the alias (a hello
    /// from an agent that reports `aarch64` would not seat) or prints it back.
    #[test]
    fn the_arch_parses_its_alias_and_prints_one_spelling() {
        assert_eq!("x86_64".parse::<Arch>(), Ok(Arch::X86_64));
        assert_eq!("arm64".parse::<Arch>(), Ok(Arch::Arm64));
        assert_eq!("aarch64".parse::<Arch>(), Ok(Arch::Arm64));
        assert_eq!(Arch::Arm64.to_string(), "arm64");
        assert_eq!(Arch::X86_64.to_string(), "x86_64");
        for bad in ["sparc", "", "ARM64", "amd64"] {
            assert_eq!(bad.parse::<Arch>(), Err(UnknownArch(bad.to_owned())));
        }
        assert_eq!(
            UnknownArch("sparc".into()).to_string(),
            "unknown architecture \"sparc\""
        );
    }

    /// Asserts the wire form of the architecture: each encodes as its name, both names and
    /// the alias `aarch64` decode, and the OCI name `amd64`, an upper case name and an
    /// unknown one fail to decode. Catches a field that encodes `Arm64` (the variant's
    /// name) or `aarch64`, which a peer that compares names would not match.
    #[test]
    fn the_arch_crosses_the_wire_by_its_name() {
        for arch in [Arch::X86_64, Arch::Arm64] {
            let text = serde_json::to_string(&arch).unwrap();
            assert_eq!(text, format!("\"{}\"", arch.name()));
            assert_eq!(serde_json::from_str::<Arch>(&text).unwrap(), arch);
        }
        let alias: Arch = serde_json::from_str("\"aarch64\"").unwrap();
        assert_eq!(alias, Arch::Arm64);
        for bad in ["\"amd64\"", "\"ARM64\"", "\"sparc\""] {
            assert!(serde_json::from_str::<Arch>(bad).is_err(), "{bad}");
        }
    }

    /// Asserts that a stated level is the highest of a list of one family, in any order,
    /// with spaces and with a level repeated, and that an empty or blank text, an empty
    /// element anywhere in the list, an unknown name or a mix of families states none. Catches a list read as its first or last entry, and a mixed list
    /// ranked on one scale.
    #[test]
    fn the_highest_listed_level_is_the_top_of_one_family() {
        let v3 = Some(IsaLevel::X86_64(X86Level::V3));
        assert_eq!(IsaLevel::highest_listed("x86-64-v3"), v3);
        assert_eq!(
            IsaLevel::highest_listed("x86-64-v1,x86-64-v2,x86-64-v3"),
            v3
        );
        assert_eq!(IsaLevel::highest_listed("x86-64-v3, x86-64-v1"), v3);
        assert_eq!(
            IsaLevel::highest_listed("armv8.0-a,armv8.2-a"),
            Some(IsaLevel::Arm64(ArmVersion::V8_2))
        );
        assert_eq!(IsaLevel::highest_listed("x86-64-v3,x86-64-v3"), v3);
        assert_eq!(
            IsaLevel::highest_listed("armv8.2-a,armv8.2-a,armv8.0-a"),
            Some(IsaLevel::Arm64(ArmVersion::V8_2))
        );
        for none in [
            "",
            " ",
            " , ",
            ",",
            ",x86-64-v3",
            "x86-64-v1,,x86-64-v3",
            "armv8.0-a, ,armv8.2-a",
            "x86-64-v9",
            "x86-64-v3,",
            "x86-64-v3,armv8.4-a",
            "armv8.4-a,x86-64-v1",
        ] {
            assert_eq!(IsaLevel::highest_listed(none), None, "{none:?}");
        }
    }
}
