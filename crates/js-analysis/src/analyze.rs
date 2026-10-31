//! Reading the run records and building the report.

use std::collections::BTreeSet;

use crate::compare::{compare, cv_compare};
use crate::describe::{min_runs, summarize};
use crate::error::Error;
use crate::types::{
    Comparison, CvComparison, GroupReport, MetricReport, Options, Pair, Report, RunPlan, RunRecord,
    WARMUP,
};

/// Reads JSON lines, one [`RunRecord`] per non-blank line, in order.
///
/// # Errors
/// [`Error::Line`] for the first line that is not a record.
pub fn parse_jsonl(text: &str) -> Result<Vec<RunRecord>, Error> {
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RunRecord = serde_json::from_str(line).map_err(|source| Error::Line {
            line: i + 1,
            source,
        })?;
        records.push(record);
    }
    Ok(records)
}

fn metric_report(group: &GroupReport, metric: &str, effects: &[f64]) -> MetricReport {
    let values = group.values(metric);
    let summary = summarize(&values);
    let plans = summary
        .as_ref()
        .and_then(|s| s.cv)
        .map_or_else(Vec::new, |cv| {
            effects
                .iter()
                .map(|&effect| RunPlan {
                    effect,
                    min_runs_ci: min_runs(cv, effect, false),
                    min_runs_power80: min_runs(cv, effect, true),
                })
                .collect()
        });
    MetricReport {
        metric: metric.to_string(),
        n: values.len(),
        n_missing: group.n_runs - values.len(),
        summary,
        plans,
    }
}

fn finish(group: &mut GroupReport, effects: &[f64]) {
    group.n_warmup = group.runs.iter().filter(|r| r.label == WARMUP).count();
    group.n_runs = group.runs.len() - group.n_warmup;
    let names: BTreeSet<&String> = group.runs.iter().flat_map(|r| r.metrics.keys()).collect();
    let metrics: Vec<MetricReport> = names
        .into_iter()
        .map(|m| metric_report(group, m, effects))
        .collect();
    group.metrics = metrics;
}

fn metric_names<'a>(a: &'a GroupReport, b: &'a GroupReport) -> BTreeSet<&'a str> {
    a.metrics
        .iter()
        .chain(&b.metrics)
        .map(|m| m.metric.as_str())
        .collect()
}

/// The pairs (base, arm) of the groups of one setting that are compared.
fn pairs<'a>(
    here: &[&'a GroupReport],
    options: &Options,
) -> Vec<(&'a GroupReport, &'a GroupReport)> {
    if options.ladder {
        return here.windows(2).map(|w| (w[0], w[1])).collect();
    }
    let base = match &options.baseline {
        Some(name) => here.iter().copied().find(|g| g.arm == *name),
        None => here.first().copied(),
    };
    match base {
        Some(b) => here
            .iter()
            .copied()
            .filter(|g| g.arm != b.arm)
            .map(|g| (b, g))
            .collect(),
        None => Vec::new(),
    }
}

/// Groups the records by setting and arm (first appearance order, run order kept) and
/// computes the report.
///
/// # Errors
/// [`Error::DuplicateRun`] when two records share setting, arm and run index;
/// [`Error::UnknownArm`] when a named base or std/mean arm has no run.
pub fn analyze(records: Vec<RunRecord>, options: &Options) -> Result<Report, Error> {
    let mut seen = BTreeSet::new();
    let mut groups: Vec<GroupReport> = Vec::new();
    for r in records {
        if !seen.insert((r.setting.clone(), r.arm.clone(), r.run_index)) {
            return Err(Error::DuplicateRun {
                setting: r.setting,
                arm: r.arm,
                run_index: r.run_index,
            });
        }
        match groups
            .iter()
            .position(|g| g.setting == r.setting && g.arm == r.arm)
        {
            Some(i) => groups[i].runs.push(r),
            None => groups.push(GroupReport {
                setting: r.setting.clone(),
                arm: r.arm.clone(),
                n_runs: 0,
                n_warmup: 0,
                runs: vec![r],
                metrics: Vec::new(),
            }),
        }
    }
    for g in &mut groups {
        finish(g, &options.effects);
    }
    let named = options
        .baseline
        .iter()
        .chain(options.cv_pairs.iter().flat_map(|(a, b)| [a, b]));
    for name in named {
        if !groups.iter().any(|g| g.arm == *name) {
            return Err(Error::UnknownArm(name.clone()));
        }
    }
    let mut settings: Vec<&String> = Vec::new();
    for g in &groups {
        if !settings.contains(&&g.setting) {
            settings.push(&g.setting);
        }
    }
    let mut comparisons: Vec<Comparison> = Vec::new();
    let mut cv_comparisons: Vec<CvComparison> = Vec::new();
    for setting in settings {
        let here: Vec<&GroupReport> = groups.iter().filter(|g| g.setting == *setting).collect();
        for (b, g) in pairs(&here, options) {
            for name in metric_names(b, g) {
                let (bv, gv) = (b.values(name), g.values(name));
                let base = Pair {
                    name: &b.arm,
                    values: &bv,
                };
                let arm = Pair {
                    name: &g.arm,
                    values: &gv,
                };
                comparisons.push(compare(setting, name, base, arm));
            }
        }
        for (an, bn) in &options.cv_pairs {
            let found = (
                here.iter().copied().find(|g| g.arm == *an),
                here.iter().copied().find(|g| g.arm == *bn),
            );
            if let (Some(b), Some(g)) = found {
                for name in metric_names(b, g) {
                    let (bv, gv) = (b.values(name), g.values(name));
                    let base = Pair {
                        name: &b.arm,
                        values: &bv,
                    };
                    let arm = Pair {
                        name: &g.arm,
                        values: &gv,
                    };
                    cv_comparisons.push(cv_compare(setting, name, base, arm));
                }
            }
        }
    }
    Ok(Report {
        groups,
        comparisons,
        cv_comparisons,
    })
}
