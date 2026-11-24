//! `--pack-limit` and `server.pack_limit`: how full the placement engine packs a host
//! before it places on the least full one, as a fraction of the host's capacity.
//!
//! The value is js-sched's own setting (`LoadConfig::pack_limit_permille`), written for the
//! operator as a fraction: `0.8` is 800 thousandths. Its default and its ceiling are
//! js-sched's ([`LoadConfig::default`], [`MAX_PACK_LIMIT_PERMILLE`]); neither is written
//! again here, so the two cannot drift.

use std::fmt;

use js_sched::{LoadConfig, MAX_PACK_LIMIT_PERMILLE};

/// The name of the key, as errors spell it.
const KEY: &str = "pack_limit";

/// The most digits after the point: thousandths.
const DECIMALS: usize = 3;

/// The pack limit of the placement engine: the thousandths of a host's capacity (the larger
/// of its CPU and memory share) up to which the engine packs the fullest host, from 0 (0.0)
/// to [`MAX_PACK_LIMIT_PERMILLE`] (1.0). The default is js-sched's, 0.8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimit(u32);

/// A `pack_limit` that is not a fraction from 0.0 to 1.0 with at most three decimals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{KEY} must be a fraction of the host's capacity from 0.0 to {max}, with at most three decimals, such as 0.8; got {given:?}",
    max = fraction(MAX_PACK_LIMIT_PERMILLE)
)]
pub struct PackLimitError {
    /// The value as given.
    pub given: String,
}

impl Default for PackLimit {
    fn default() -> Self {
        Self(LoadConfig::default().pack_limit_permille)
    }
}

impl PackLimit {
    /// The limit `text` writes as a fraction: digits, and optionally a point and up to three
    /// more digits, from `0.0` to `1.0`. A sign, a space, an exponent or an empty text is
    /// refused.
    ///
    /// # Errors
    /// [`PackLimitError`] for anything else.
    pub fn parse(text: &str) -> Result<Self, PackLimitError> {
        let refuse = || PackLimitError {
            given: text.to_owned(),
        };
        let (whole, rest) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        let shaped = [
            !whole.is_empty(),
            digits(whole),
            digits(rest),
            rest.len() <= DECIMALS,
            !text.ends_with('.'),
        ];
        if !shaped.into_iter().all(|ok| ok) {
            return Err(refuse());
        }
        let padded = format!("{rest:0<DECIMALS$}");
        let whole: u64 = whole.parse().map_err(|_| refuse())?;
        // Three ASCII digits, checked above: the parse cannot fail.
        let thousandths: u64 = padded.parse().unwrap_or(0);
        let total = whole.saturating_mul(1000).saturating_add(thousandths);
        match u32::try_from(total) {
            Ok(total) if total <= MAX_PACK_LIMIT_PERMILLE => Ok(Self(total)),
            _ => Err(refuse()),
        }
    }

    /// The thousandths of capacity: js-sched's `pack_limit_permille`.
    pub fn permille(self) -> u32 {
        self.0
    }

    /// `load` with this pack limit and every other setting as it was.
    pub(crate) fn apply(self, load: LoadConfig) -> LoadConfig {
        LoadConfig {
            pack_limit_permille: self.0,
            ..load
        }
    }
}

impl fmt::Display for PackLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&fraction(self.0))
    }
}

/// `permille` thousandths as a fraction, with at least one decimal and no trailing zero
/// after it: 800 is `0.8`, 1000 is `1.0`, 5 is `0.005`.
fn fraction(permille: u32) -> String {
    let decimals = format!("{:03}", permille % 1000);
    let decimals = decimals.trim_end_matches('0');
    let decimals = if decimals.is_empty() { "0" } else { decimals };
    format!("{}.{decimals}", permille / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Behaviour: the default is js-sched's and a fraction is read as thousandths of the
    /// capacity. Catches: the default restated in this crate and drifting from js-sched's,
    /// and a fraction read off by a factor of ten.
    #[test]
    fn the_default_is_js_scheds_and_values_are_thousandths_of_the_capacity() {
        let default = LoadConfig::default().pack_limit_permille;
        assert_eq!(PackLimit::default().permille(), default);
        assert_eq!(
            PackLimit::default().apply(LoadConfig::default()),
            LoadConfig::default()
        );
        let cases = [
            ("0", 0),
            ("0.0", 0),
            ("0.8", 800),
            ("0.80", 800),
            ("0.005", 5),
            ("0.999", 999),
            ("1", 1000),
            ("1.000", 1000),
        ];
        for (text, permille) in cases {
            assert_eq!(
                PackLimit::parse(text).map(PackLimit::permille),
                Ok(permille),
                "{text}"
            );
        }
        let applied = PackLimit::parse("0.65")
            .unwrap()
            .apply(LoadConfig::default());
        assert_eq!(applied.pack_limit_permille, 650);
        assert_eq!(applied.s_cpu_permille, LoadConfig::default().s_cpu_permille);
    }

    /// Behaviour: a value above 1.0, or not a plain decimal, is refused and not clamped,
    /// and each refusal names the key and the value exactly. Catches: a limit above the
    /// ceiling, or a sign, space, exponent or trailing point, accepted.
    #[test]
    fn values_outside_zero_to_one_or_not_decimals_are_refused() {
        let bad = [
            "",
            "1.001",
            "1.5",
            "2",
            "abc",
            "1.",
            ".5",
            "0.8001",
            "-0.8",
            "+0.8",
            " 0.8",
            "0.8 ",
            "8e-1",
            "0,8",
            "0..8",
            "0.a",
            "NaN",
            "inf",
            "99999999999999999999",
        ];
        for text in bad {
            let err = PackLimit::parse(text).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "pack_limit must be a fraction of the host's capacity from 0.0 to 1.0, with at most three decimals, such as 0.8; got {text:?}"
                ),
            );
        }
    }

    /// Behaviour: a limit prints as its fraction, with one decimal at least and no trailing
    /// zeros. Catches: trailing zeros, no decimal, or off by a factor of ten.
    #[test]
    fn a_limit_prints_as_its_fraction() {
        let shown = |text: &str| PackLimit::parse(text).unwrap().to_string();
        assert_eq!(shown("0"), "0.0");
        assert_eq!(shown("0.8"), "0.8");
        assert_eq!(shown("0.005"), "0.005");
        assert_eq!(shown("0.250"), "0.25");
        assert_eq!(shown("1"), "1.0");
        assert_eq!(PackLimit::default().to_string(), "0.8");
    }

    /// A server configuration from the flags `extra` beside one pool, as the program
    /// checks them.
    fn config_with(dir: &std::path::Path, extra: &[&str]) -> crate::config::Config {
        use std::os::unix::fs::PermissionsExt;
        let write = |name: &str, text: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path.display().to_string()
        };
        let pool = "name = \"burst\"\nclass = \"burst-host\"\nmax_machines = 2\n\n\
                    [shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\n\n\
                    [capabilities]\narch = \"x86_64\"\n";
        let mut args = vec![
            "js-lease-server".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            format!("--state-file={}", dir.join("state").display()),
            "--boot=initialise".to_owned(),
            format!("--token-file={}", write("token", &"t".repeat(40), 0o600)),
            format!("--agent-pool-config={}", write("pool.toml", pool, 0o644)),
        ];
        args.extend(extra.iter().map(|flag| (*flag).to_owned()));
        let cli = crate::config::Cli::try_parse_from(args).unwrap();
        crate::config::Config::from_cli(cli).unwrap()
    }

    /// Asserts that the server's configuration carries `--pack-limit` as given, and
    /// js-sched's default without it. Catches a flag read and then dropped before the
    /// configuration the engine is built from.
    #[test]
    fn the_configuration_carries_the_flag_or_the_default() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/pack_limit");
        let dir = js_testkit::dir::TestDir::under(root, "flag");
        let given = config_with(dir.path(), &["--pack-limit=0.65"]);
        assert_eq!(given.pack_limit().permille(), 650);
        let dir = js_testkit::dir::TestDir::under(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/pack_limit"),
            "default",
        );
        assert_eq!(
            config_with(dir.path(), &[]).pack_limit(),
            PackLimit::default()
        );
    }
}
