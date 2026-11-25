//! `limit:<what>`: the resource limits the job runs under (file size, processes,
//! descriptors, memory), each [`Outcome::Limited`] or [`Outcome::Unlimited`] by its soft value.

use rustix::process::{Resource, Rlimit, getrlimit};

use crate::outcome::{Line, Outcome};

/// The limits a probe reports, by name.
const LIMITS: [(&str, Resource); 6] = [
    ("file-size", Resource::Fsize),
    ("processes", Resource::Nproc),
    ("descriptors", Resource::Nofile),
    ("memory", Resource::As),
    ("data", Resource::Data),
    ("resident", Resource::Rss),
];

/// One line per limit, as this process has them.
#[must_use]
pub fn lines() -> Vec<Line> {
    LIMITS
        .iter()
        .map(|(name, resource)| line(name, &getrlimit(*resource)))
        .collect()
}

fn line(name: &str, limit: &Rlimit) -> Line {
    let show = |v: Option<u64>| v.map_or_else(|| "unlimited".to_owned(), |n| n.to_string());
    let outcome = if limit.current.is_some() {
        Outcome::Limited
    } else {
        Outcome::Unlimited
    };
    Line::new(format!("limit:{name}"), outcome).detail(format!(
        "soft {}, hard {}",
        show(limit.current),
        show(limit.maximum)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finite_soft_value_is_limited_and_an_infinite_one_unlimited() {
        let finite = Rlimit {
            current: Some(256),
            maximum: None,
        };
        assert_eq!(
            line("descriptors", &finite).to_string(),
            "limit:descriptors: limited; soft 256, hard unlimited"
        );
        let infinite = Rlimit {
            current: None,
            maximum: None,
        };
        assert_eq!(
            line("memory", &infinite).to_string(),
            "limit:memory: unlimited; soft unlimited, hard unlimited"
        );
    }

    #[test]
    fn every_limit_is_reported_by_name() {
        let names: Vec<String> = lines().into_iter().map(|l| l.name).collect();
        assert_eq!(
            names,
            [
                "limit:file-size",
                "limit:processes",
                "limit:descriptors",
                "limit:memory",
                "limit:data",
                "limit:resident"
            ]
        );
    }
}
